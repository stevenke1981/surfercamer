//! HTTP sources: MJPEG streams and polled JPEG snapshots (basic auth).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use ss_core::{CameraId, Frame, Result};
use tokio::time::Interval;

use crate::jpeg;
use crate::{camera_error, make_frame, FrameSource};

/// Connect timeout for every HTTP request, and the bound on waiting for response headers.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on reading a snapshot body.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a reqwest client with the given connect timeout.
pub(crate) fn build_client(connect_timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|e| camera_error(format!("cannot create HTTP client: {e}")))
}

/// GET `url` with optional basic auth. Non-2xx statuses and timeouts map to `Error::Camera`.
pub(crate) async fn get_with_auth(
    client: &reqwest::Client,
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<reqwest::Response> {
    let mut request = client.get(url);
    if let Some(user) = username.filter(|u| !u.is_empty()) {
        request = request.basic_auth(user, password);
    }
    let response = tokio::time::timeout(CONNECT_TIMEOUT, request.send())
        .await
        .map_err(|_| camera_error("connect timeout"))?
        .map_err(|e| camera_error(format!("request failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(camera_error(format!("HTTP status {status}")));
    }
    Ok(response)
}

/// Owned basic-auth credentials for repeated requests.
#[derive(Clone)]
struct Auth {
    username: String,
    password: Option<String>,
}

fn owned_auth(username: Option<&str>, password: Option<&str>) -> Option<Auth> {
    username.filter(|u| !u.is_empty()).map(|u| Auth {
        username: u.to_string(),
        password: password.map(str::to_string),
    })
}

/// Polls a URL that returns one JPEG per request.
pub struct SnapshotSource {
    camera_id: CameraId,
    client: reqwest::Client,
    url: String,
    auth: Option<Auth>,
    period: Duration,
    /// Created on first use so that construction does not require a runtime.
    ticker: Option<Interval>,
}

impl SnapshotSource {
    pub fn new(
        camera_id: CameraId,
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        interval: Duration,
    ) -> Result<Self> {
        Ok(Self {
            camera_id,
            client: build_client(CONNECT_TIMEOUT)?,
            url: url.to_string(),
            auth: owned_auth(username, password),
            // tokio::time::interval panics on a zero period.
            period: interval.max(Duration::from_millis(1)),
            ticker: None,
        })
    }

    async fn fetch(&self) -> Result<Bytes> {
        let mut request = self.client.get(&self.url);
        if let Some(auth) = &self.auth {
            request = request.basic_auth(&auth.username, auth.password.as_deref());
        }
        let response = tokio::time::timeout(CONNECT_TIMEOUT, request.send())
            .await
            .map_err(|_| camera_error("snapshot request timeout"))?
            .map_err(|e| camera_error(format!("snapshot request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(camera_error(format!("HTTP status {status}")));
        }
        tokio::time::timeout(BODY_TIMEOUT, response.bytes())
            .await
            .map_err(|_| camera_error("snapshot body timeout"))?
            .map_err(|e| camera_error(format!("snapshot body failed: {e}")))
    }
}

#[async_trait]
impl FrameSource for SnapshotSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        let period = self.period;
        let ticker = self.ticker.get_or_insert_with(|| {
            let mut t = tokio::time::interval(period);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        });
        ticker.tick().await;
        let body = self.fetch().await?;
        if jpeg::dimensions(&body).is_none() {
            return Err(camera_error("snapshot is not a JPEG image"));
        }
        Ok(make_frame(self.camera_id, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_auth_requires_username() {
        assert!(owned_auth(None, Some("pw")).is_none());
        assert!(owned_auth(Some(""), None).is_none());
        let a = owned_auth(Some("u"), None).expect("auth");
        assert_eq!(a.username, "u");
        assert_eq!(a.password, None);
    }

    #[test]
    fn snapshot_construction_does_not_need_runtime() {
        let s = SnapshotSource::new(1, "http://127.0.0.1:1/x.jpg", None, None, Duration::ZERO)
            .expect("new");
        assert!(s.period >= Duration::from_millis(1));
    }
}

#[cfg(test)]
mod http_tests {
    use ss_core::Error;

    use super::test_support::serve;
    use super::*;
    use crate::jpeg;
    use crate::mjpeg::MjpegHttpSource;
    use crate::synthetic::SyntheticSource;

    /// Lower-case because HTTP clients may send header names in any case.
    /// "user:pass" in base64 is "dXNlcjpwYXNz".
    const BASIC_USER_PASS: &str = "authorization: basic dxnlcjpwyxnz";

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn sample_jpeg(n: u64) -> Vec<u8> {
        SyntheticSource::new(1, 64, 48, 10).render(n)
    }

    fn mjpeg_body(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames {
            body.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n");
            body.extend_from_slice(f);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--frame--\r\n");
        body
    }

    #[tokio::test]
    async fn mjpeg_stream_yields_frames_then_reports_end() {
        let frames = vec![sample_jpeg(0), sample_jpeg(1)];
        let body = mjpeg_body(&frames);
        let base = serve(move |_| {
            response(
                "200 OK",
                "Content-Type: multipart/x-mixed-replace; boundary=frame\r\n",
                &body,
            )
        })
        .await;
        let mut src = MjpegHttpSource::connect(5, &format!("{base}/video"), None, None)
            .await
            .expect("connect");
        for expected in &frames {
            let frame = src.next_frame().await.expect("frame");
            assert_eq!(frame.camera_id, 5);
            assert_eq!(frame.jpeg.as_ref(), expected.as_slice());
            assert_eq!((frame.width, frame.height), (64, 48));
        }
        match src.next_frame().await {
            Err(Error::Camera(msg)) => assert!(msg.contains("ended"), "{msg}"),
            other => panic!("expected stream end, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mjpeg_sends_basic_auth() {
        let body = mjpeg_body(&[sample_jpeg(0)]);
        let base = serve(move |head| {
            if head.to_ascii_lowercase().contains(BASIC_USER_PASS) {
                response(
                    "200 OK",
                    "Content-Type: multipart/x-mixed-replace; boundary=frame\r\n",
                    &body,
                )
            } else {
                response(
                    "401 Unauthorized",
                    "WWW-Authenticate: Basic realm=\"cam\"\r\n",
                    b"",
                )
            }
        })
        .await;
        let mut src =
            MjpegHttpSource::connect(1, &format!("{base}/video"), Some("user"), Some("pass"))
                .await
                .expect("connect with auth");
        assert!(src.next_frame().await.is_ok());
    }

    #[tokio::test]
    async fn mjpeg_401_is_camera_error() {
        let base = serve(|_| response("401 Unauthorized", "", b"")).await;
        match MjpegHttpSource::connect(1, &format!("{base}/video"), None, None).await {
            Err(Error::Camera(msg)) => assert!(msg.contains("401"), "{msg}"),
            Err(other) => panic!("wrong error {other:?}"),
            Ok(_) => panic!("401 must fail"),
        }
    }

    #[tokio::test]
    async fn snapshot_polls_jpegs() {
        let body = sample_jpeg(3);
        let served = body.clone();
        let base =
            serve(move |_| response("200 OK", "Content-Type: image/jpeg\r\n", &served)).await;
        let mut src = SnapshotSource::new(
            2,
            &format!("{base}/snap.jpg"),
            None,
            None,
            Duration::from_millis(20),
        )
        .expect("new");
        let a = src.next_frame().await.expect("first");
        let b = src.next_frame().await.expect("second");
        assert_eq!(a.jpeg.as_ref(), body.as_slice());
        assert_eq!(b.camera_id, 2);
        assert_eq!((a.width, a.height), (64, 48));
    }

    #[tokio::test]
    async fn snapshot_rejects_non_jpeg_body() {
        let base = serve(|_| {
            response(
                "200 OK",
                "Content-Type: text/html\r\n",
                b"<html>login</html>",
            )
        })
        .await;
        let mut src = SnapshotSource::new(
            2,
            &format!("{base}/snap"),
            None,
            None,
            Duration::from_millis(20),
        )
        .expect("new");
        match src.next_frame().await {
            Err(Error::Camera(msg)) => assert!(msg.contains("not a JPEG"), "{msg}"),
            other => panic!("expected camera error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_sends_basic_auth_and_reports_401() {
        let body = sample_jpeg(0);
        let base = serve(move |head| {
            if head.to_ascii_lowercase().contains(BASIC_USER_PASS) {
                response("200 OK", "Content-Type: image/jpeg\r\n", &body)
            } else {
                response("401 Unauthorized", "", b"")
            }
        })
        .await;
        let mut ok = SnapshotSource::new(
            2,
            &format!("{base}/s"),
            Some("user"),
            Some("pass"),
            Duration::from_millis(20),
        )
        .expect("new");
        assert!(ok.next_frame().await.is_ok());
        let mut denied = SnapshotSource::new(
            2,
            &format!("{base}/s"),
            Some("user"),
            Some("wrong"),
            Duration::from_millis(20),
        )
        .expect("new");
        assert!(matches!(denied.next_frame().await, Err(Error::Camera(_))));
        assert!(jpeg::dimensions(&sample_jpeg(0)).is_some());
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Tiny raw-HTTP mock server for tests (no external network).

    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves `handler(request_head)` as the raw response bytes for every connection.
    /// Returns the base URL, e.g. `http://127.0.0.1:PORT`.
    pub async fn serve<F>(handler: F) -> String
    where
        F: Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match sock.read(&mut byte).await {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let response = handler(&text);
                    let _ = sock.write_all(&response).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }
}
