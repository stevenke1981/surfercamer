//! `/cameras/*` and `/discovery/onvif`.

use std::mem::discriminant;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use ss_core::redact::{scrub_credentials, strip_userinfo, MASK};
use ss_core::{Camera, CameraId, CameraInput, CameraSource, CameraStatus, RecordingQuery};

use crate::auth::{Manager, Viewer};
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::stream;
use crate::AppState;

/// Placeholder returned for stored passwords; sending it back keeps the stored value.
pub const PASSWORD_MASK: &str = MASK;
const LIST_PAGE: u32 = 1000;
/// Snapshots older than this are reported as unavailable.
const SNAPSHOT_MAX_AGE_SECS: i64 = 10;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/cameras", get(list_cameras).post(create_camera))
        .route(
            "/cameras/{id}",
            get(get_camera).put(update_camera).delete(delete_camera),
        )
        .route("/cameras/{id}/enable", post(enable_camera))
        .route("/cameras/{id}/disable", post(disable_camera))
        .route("/cameras/{id}/snapshot", get(snapshot))
        .route("/cameras/{id}/live", get(live))
        .route("/cameras/{id}/record/start", post(record_start))
        .route("/cameras/{id}/record/stop", post(record_stop))
        .route("/discovery/onvif", get(discover))
}

/// Camera as returned by the API: credentials redacted, runtime status attached.
#[derive(Serialize)]
pub struct CameraView {
    #[serde(flatten)]
    pub camera: Camera,
    pub status: CameraStatus,
}

fn view(st: &AppState, mut camera: Camera) -> CameraView {
    camera.source = camera.source.redacted();
    let mut status = st.engine.status(camera.id);
    scrub_status(&mut status);
    CameraView { camera, status }
}

/// Error text can echo the camera URL (ffmpeg, HTTP clients); never return credentials.
fn scrub_status(status: &mut CameraStatus) {
    if let Some(err) = status.last_error.as_mut() {
        let scrubbed = scrub_credentials(err.as_str());
        *err = scrubbed;
    }
}

/// `scheme://user:pass@host/…` split into percent-decoded `(user, pass)`.
/// `None` when the URL carries no userinfo.
fn url_userinfo(url: &str) -> Option<(Option<String>, Option<String>)> {
    let scheme_end = url.find("://")?;
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..authority_end].rfind('@')?;
    let info = &rest[..at];
    let (user, pass) = match info.split_once(':') {
        Some((user, pass)) => (user, Some(pass)),
        None => (info, None),
    };
    let user = Some(percent_decode(user)).filter(|u| !u.is_empty());
    Some((user, pass.map(percent_decode)))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode `%XX` escapes; malformed escapes are kept as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn url_of(src: &CameraSource) -> Option<&str> {
    match src {
        CameraSource::Rtsp { url, .. }
        | CameraSource::Mjpeg { url, .. }
        | CameraSource::Snapshot { url, .. } => Some(url.as_str()),
        CameraSource::Synthetic { .. } => None,
    }
}

/// `(scheme, host, port)` of a URL, lowercased, default ports filled in.
fn origin(url: &str) -> Option<(String, String, u16)> {
    let scheme_end = url.find("://")?;
    let scheme = url[..scheme_end].to_ascii_lowercase();
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            "rtsp" => 554,
            "rtsps" => 322,
            _ => return None,
        },
    };
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// Same source kind, scheme, host and port: the stored password may be reused.
fn same_target(stored: &CameraSource, input: &CameraSource) -> bool {
    if discriminant(stored) != discriminant(input) {
        return false;
    }
    let old = url_of(stored).and_then(origin);
    old.is_some() && old == url_of(input).and_then(origin)
}

/// Move `user:pass@` out of the URL into the credential fields; the stored
/// URL never contains credentials. Userinfo in the URL wins over the fields.
fn split_credentials(src: &mut CameraSource) {
    let (url, username, password) = match src {
        CameraSource::Rtsp {
            url,
            username,
            password,
        }
        | CameraSource::Mjpeg {
            url,
            username,
            password,
        }
        | CameraSource::Snapshot {
            url,
            username,
            password,
            ..
        } => (url, username, password),
        CameraSource::Synthetic { .. } => return,
    };
    if let Some((user, pass)) = url_userinfo(url) {
        if user.is_some() {
            *username = user;
        }
        if pass.is_some() {
            *password = pass;
        }
        let bare = strip_userinfo(url);
        *url = bare;
    }
}

fn password_slot(src: &mut CameraSource) -> Option<&mut Option<String>> {
    match src {
        CameraSource::Rtsp { password, .. }
        | CameraSource::Mjpeg { password, .. }
        | CameraSource::Snapshot { password, .. } => Some(password),
        CameraSource::Synthetic { .. } => None,
    }
}

fn stored_password(src: &CameraSource) -> Option<String> {
    match src {
        CameraSource::Rtsp { password, .. }
        | CameraSource::Mjpeg { password, .. }
        | CameraSource::Snapshot { password, .. } => password.clone(),
        CameraSource::Synthetic { .. } => None,
    }
}

/// Resolve a masked (`********`) password. It keeps the stored password only
/// while the camera still points at the same scheme, host and port; on a new
/// camera there is nothing stored, so the mask clears the password.
fn restore_masked_password(
    input: &mut CameraSource,
    stored: Option<&CameraSource>,
) -> ApiResult<()> {
    if stored_password(input).as_deref() != Some(PASSWORD_MASK) {
        return Ok(());
    }
    let replacement = match stored {
        None => None,
        Some(old) if same_target(old, input) => stored_password(old),
        Some(_) => {
            return Err(ApiError::invalid(
                "re-enter the password when changing the camera address",
            ));
        }
    };
    if let Some(slot) = password_slot(input) {
        *slot = replacement;
    }
    Ok(())
}

async fn list_cameras(State(st): State<AppState>, _v: Viewer) -> ApiResult<Json<Vec<CameraView>>> {
    let cameras = st.store.list_cameras()?;
    Ok(Json(cameras.into_iter().map(|c| view(&st, c)).collect()))
}

async fn get_camera(
    State(st): State<AppState>,
    _v: Viewer,
    Path(id): Path<CameraId>,
) -> ApiResult<Json<CameraView>> {
    let camera = st.store.get_camera(id)?;
    Ok(Json(view(&st, camera)))
}

async fn create_camera(
    State(st): State<AppState>,
    _m: Manager,
    ApiJson(mut input): ApiJson<CameraInput>,
) -> ApiResult<Response> {
    split_credentials(&mut input.source);
    restore_masked_password(&mut input.source, None)?;
    input.validate()?;
    let camera = st.store.create_camera(&input)?;
    st.engine.reload_camera(camera.id).await?;
    Ok((StatusCode::CREATED, Json(view(&st, camera))).into_response())
}

async fn update_camera(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
    ApiJson(mut input): ApiJson<CameraInput>,
) -> ApiResult<Json<CameraView>> {
    let existing = st.store.get_camera(id)?;
    split_credentials(&mut input.source);
    restore_masked_password(&mut input.source, Some(&existing.source))?;
    input.validate()?;
    let camera = st.store.update_camera(id, &input)?;
    st.engine.reload_camera(id).await?;
    Ok(Json(view(&st, camera)))
}

/// Delete all segment files of a camera, page by page (blocking I/O).
fn remove_camera_files(st: &AppState, id: CameraId) -> ApiResult<()> {
    let root = st.engine.storage_root().to_path_buf();
    let mut offset: u32 = 0;
    loop {
        let batch = st.store.list_recordings(&RecordingQuery {
            camera_id: Some(id),
            limit: Some(LIST_PAGE),
            offset: Some(offset),
            ..Default::default()
        })?;
        for rec in &batch {
            ss_recorder::retention::delete_segment_files(&root, &rec.path)?;
        }
        let n = batch.len() as u32;
        if n < LIST_PAGE {
            return Ok(());
        }
        offset = offset.saturating_add(n);
    }
}

async fn delete_camera(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
) -> ApiResult<StatusCode> {
    st.store.get_camera(id)?;
    // Stop the worker first so nothing writes segments while they are removed.
    st.engine.stop_camera(id).await;
    let worker = st.clone();
    let removed = match tokio::task::spawn_blocking(move || remove_camera_files(&worker, id)).await
    {
        Ok(result) => result,
        Err(e) => Err(ApiError::from(e)),
    };
    if let Err(e) = removed {
        // The camera still exists: put its worker back before reporting.
        if let Err(re) = st.engine.reload_camera(id).await {
            tracing::warn!(camera_id = id, error = %re, "worker restart after failed camera delete failed");
        }
        return Err(e);
    }
    st.store.delete_camera(id)?;
    st.engine.frames().remove(id);
    Ok(StatusCode::NO_CONTENT)
}

async fn set_enabled(st: &AppState, id: CameraId, enabled: bool) -> ApiResult<Json<CameraView>> {
    let cam = st.store.get_camera(id)?;
    let input = CameraInput {
        name: cam.name,
        enabled,
        source: cam.source,
        recording: cam.recording,
        motion: cam.motion,
    };
    let updated = st.store.update_camera(id, &input)?;
    st.engine.reload_camera(id).await?;
    Ok(Json(view(st, updated)))
}

async fn enable_camera(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
) -> ApiResult<Json<CameraView>> {
    set_enabled(&st, id, true).await
}

async fn disable_camera(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
) -> ApiResult<Json<CameraView>> {
    set_enabled(&st, id, false).await
}

async fn snapshot(
    State(st): State<AppState>,
    _v: Viewer,
    Path(id): Path<CameraId>,
) -> ApiResult<Response> {
    st.store.get_camera(id)?;
    let frame = st
        .engine
        .frames()
        .latest(id)
        .ok_or_else(|| ApiError::unavailable("no frame available yet"))?;
    if Utc::now() - frame.ts > chrono::TimeDelta::seconds(SNAPSHOT_MAX_AGE_SECS) {
        return Err(ApiError::unavailable(
            "latest frame is too old; the camera may be offline",
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        frame.jpeg.clone(),
    )
        .into_response())
}

async fn live(
    State(st): State<AppState>,
    Viewer(me): Viewer,
    Path(id): Path<CameraId>,
) -> ApiResult<Response> {
    st.store.get_camera(id)?;
    let frames = stream::live_stream(st.engine.frames(), id);
    Ok(stream::mjpeg_response(stream::bounded(
        &st, me.token, frames,
    )))
}

async fn record_start(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
) -> ApiResult<StatusCode> {
    st.store.get_camera(id)?;
    st.engine.set_manual_recording(id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn record_stop(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<CameraId>,
) -> ApiResult<StatusCode> {
    st.store.get_camera(id)?;
    st.engine.set_manual_recording(id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DiscoveryQuery {
    pub timeout_ms: Option<u64>,
}

async fn discover(
    _m: Manager,
    ApiQuery(q): ApiQuery<DiscoveryQuery>,
) -> ApiResult<Json<Vec<ss_camera::onvif::DiscoveredDevice>>> {
    let timeout = Duration::from_millis(q.timeout_ms.unwrap_or(3000).clamp(500, 10_000));
    Ok(Json(ss_camera::onvif::discover(timeout).await?))
}

#[cfg(test)]
mod tests {
    use ss_core::ConnectionState;

    use super::*;

    #[test]
    fn status_error_text_is_scrubbed() {
        let mut status = CameraStatus {
            camera_id: 1,
            state: ConnectionState::Offline,
            recording: false,
            recording_trigger: None,
            motion_active: false,
            fps: 0.0,
            last_frame_at: None,
            last_error: Some("open rtsp://admin:s3cret@10.0.0.5/live failed".into()),
        };
        scrub_status(&mut status);
        assert_eq!(
            status.last_error.as_deref(),
            Some("open rtsp://10.0.0.5/live failed")
        );
    }

    #[test]
    fn userinfo_is_split_and_percent_decoded() {
        assert_eq!(
            url_userinfo("rtsp://a%40b:p%2Fw%25@h:554/s"),
            Some((Some("a@b".into()), Some("p/w%".into())))
        );
        assert_eq!(
            url_userinfo("rtsp://:only@h/s"),
            Some((None, Some("only".into())))
        );
        assert_eq!(url_userinfo("http://h/a@b"), None);
    }

    #[test]
    fn origin_compares_scheme_host_and_port() {
        assert_eq!(
            origin("HTTP://Cam.local/x"),
            Some(("http".into(), "cam.local".into(), 80))
        );
        assert_eq!(
            origin("https://h/x"),
            Some(("https".into(), "h".into(), 443))
        );
        assert_eq!(
            origin("rtsp://u:p@h:8554/x"),
            Some(("rtsp".into(), "h".into(), 8554))
        );
        assert_eq!(
            origin("http://[::1]:8080/x"),
            Some(("http".into(), "::1".into(), 8080))
        );
        let a = CameraSource::Mjpeg {
            url: "http://h:9/a".into(),
            username: None,
            password: None,
        };
        let b = CameraSource::Mjpeg {
            url: "http://h:9/b?q=1".into(),
            username: None,
            password: None,
        };
        let c = CameraSource::Snapshot {
            url: "http://h:9/a".into(),
            username: None,
            password: None,
            interval_ms: 1000,
        };
        assert!(same_target(&a, &b));
        assert!(!same_target(&a, &c), "different kind");
    }
}
