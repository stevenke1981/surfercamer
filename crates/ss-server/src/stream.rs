//! MJPEG (`multipart/x-mixed-replace`) bodies for live view and playback.
//! SPEC §10: boundary `ssframe`, one JPEG per part.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{stream, Stream, StreamExt};
use ss_core::{CameraId, FrameHub, Store};
use ss_recorder::segment::SegmentReader;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::AppState;

pub const BOUNDARY: &str = "ssframe";

/// Resolves when `rx` holds `true`. If the sender is gone it never resolves
/// (a dropped sender must not look like a shutdown).
pub async fn wait_true(mut rx: watch::Receiver<bool>) {
    let reached = rx.wait_for(|v| *v).await.map(|_| ());
    if reached.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves when the session `token` is gone or expired, checked every `period`.
async fn session_gone(store: Arc<dyn Store>, token: String, period: Duration) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        tick.tick().await;
        if store.get_session(&token).is_err() {
            return;
        }
    }
}

/// Resolves when the server shuts down or the caller's session ends.
pub fn end_signal(st: &AppState, token: String) -> impl Future<Output = ()> + Send + 'static {
    let shutdown = st.shutdown.clone();
    let store = st.store.clone();
    let period = st.session_recheck;
    async move {
        tokio::select! {
            () = wait_true(shutdown) => tracing::debug!("stream ended: shutting down"),
            () = session_gone(store, token, period) => tracing::debug!("stream ended: session gone"),
        }
    }
}

/// Wrap a long-lived response body so it ends on shutdown or when the
/// session `token` stops being valid.
pub fn bounded<S>(
    st: &AppState,
    token: String,
    body: S,
) -> impl Stream<Item = S::Item> + Send + 'static
where
    S: Stream + Send + 'static,
{
    body.take_until(end_signal(st, token))
}

/// One multipart part carrying `jpeg`.
pub fn part(jpeg: &[u8]) -> Bytes {
    let header = format!(
        "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        jpeg.len()
    );
    let mut buf = BytesMut::with_capacity(header.len() + jpeg.len() + 2);
    buf.put_slice(header.as_bytes());
    buf.put_slice(jpeg);
    buf.put_slice(b"\r\n");
    buf.freeze()
}

/// Wrap a part stream as an MJPEG HTTP response.
pub fn mjpeg_response<S>(parts: S) -> Response
where
    S: Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    (
        [
            (
                header::CONTENT_TYPE,
                format!("multipart/x-mixed-replace; boundary={BOUNDARY}"),
            ),
            (
                header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate".to_string(),
            ),
        ],
        Body::from_stream(parts),
    )
        .into_response()
}

/// Latest frame first (if any), then every new frame. Lagged receivers keep
/// going; the stream ends when the camera's channel closes.
pub fn live_stream(
    hub: &FrameHub,
    id: CameraId,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    let rx = hub.subscribe(id);
    let latest = hub.latest(id);
    let head = stream::iter(latest.map(|f| Ok::<Bytes, Infallible>(part(&f.jpeg))));
    let tail = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(frame) => return Some((Ok::<Bytes, Infallible>(part(&frame.jpeg)), rx)),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    head.chain(tail)
}

struct PlayState {
    reader: SegmentReader,
    idx: usize,
    base_ms: u32,
    t0: tokio::time::Instant,
    speed: f64,
}

/// Replay a segment from frame `start`, pacing each frame by its index
/// timestamp divided by `speed`. Ends after the last frame.
pub fn playback_stream(
    reader: SegmentReader,
    start: usize,
    speed: f64,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    let base_ms = reader.entries().get(start).map_or(0, |e| e.offset_ms);
    let state = PlayState {
        reader,
        idx: start,
        base_ms,
        t0: tokio::time::Instant::now(),
        speed,
    };
    stream::unfold(state, |st| async move {
        let PlayState {
            reader,
            idx,
            base_ms,
            t0,
            speed,
        } = st;
        let entry = *reader.entries().get(idx)?;
        let delta_ms = f64::from(entry.offset_ms.saturating_sub(base_ms)) / speed;
        tokio::time::sleep_until(t0 + Duration::from_secs_f64(delta_ms / 1000.0)).await;
        let (reader, res) = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let res = reader.read_frame(idx);
            (reader, res)
        })
        .await
        .ok()?;
        match res {
            Ok(jpeg) => {
                let next = PlayState {
                    reader,
                    idx: idx + 1,
                    base_ms,
                    t0,
                    speed,
                };
                Some((Ok(part(&jpeg)), next))
            }
            Err(e) => {
                tracing::warn!(error = %e, frame = idx, "playback read failed; ending stream");
                None
            }
        }
    })
}
