//! `/cameras/*` and `/discovery/onvif`.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use ss_core::{Camera, CameraId, CameraInput, CameraSource, CameraStatus, RecordingQuery};

use crate::auth::{Manager, Viewer};
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::stream;
use crate::AppState;

/// Placeholder returned for stored passwords; sending it back keeps the stored value.
pub const PASSWORD_MASK: &str = "********";
const LIST_PAGE: u32 = 1000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/cameras", get(list_cameras).post(create_camera))
        .route("/cameras/{id}", get(get_camera).put(update_camera).delete(delete_camera))
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
    let status = st.engine.status(camera.id);
    CameraView { camera, status }
}

/// Stored password for the same source kind, if any.
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

/// Replace a masked password with the stored one (same kind), else clear it.
fn restore_masked_password(input: &mut CameraSource, stored: Option<&CameraSource>) {
    let same_kind = stored.is_some_and(|s| std::mem::discriminant(s) == std::mem::discriminant(input));
    let replacement = match stored {
        Some(s) if same_kind => stored_password(s),
        _ => None,
    };
    if let Some(slot) = password_slot(input) {
        if slot.as_deref() == Some(PASSWORD_MASK) {
            *slot = replacement;
        }
    }
}

async fn list_cameras(State(st): State<AppState>, _v: Viewer) -> ApiResult<Json<Vec<CameraView>>> {
    let cameras = st.store.list_cameras()?;
    Ok(Json(cameras.into_iter().map(|c| view(&st, c)).collect()))
}

async fn get_camera(State(st): State<AppState>, _v: Viewer, Path(id): Path<CameraId>) -> ApiResult<Json<CameraView>> {
    let camera = st.store.get_camera(id)?;
    Ok(Json(view(&st, camera)))
}

async fn create_camera(
    State(st): State<AppState>,
    _m: Manager,
    ApiJson(mut input): ApiJson<CameraInput>,
) -> ApiResult<Response> {
    restore_masked_password(&mut input.source, None);
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
    restore_masked_password(&mut input.source, Some(&existing.source));
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

async fn delete_camera(State(st): State<AppState>, _m: Manager, Path(id): Path<CameraId>) -> ApiResult<StatusCode> {
    st.store.get_camera(id)?;
    let worker = st.clone();
    tokio::task::spawn_blocking(move || remove_camera_files(&worker, id)).await??;
    st.store.delete_camera(id)?;
    if let Err(e) = st.engine.reload_camera(id).await {
        tracing::warn!(camera_id = id, error = %e, "worker stop after camera delete failed");
    }
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

async fn enable_camera(State(st): State<AppState>, _m: Manager, Path(id): Path<CameraId>) -> ApiResult<Json<CameraView>> {
    set_enabled(&st, id, true).await
}

async fn disable_camera(State(st): State<AppState>, _m: Manager, Path(id): Path<CameraId>) -> ApiResult<Json<CameraView>> {
    set_enabled(&st, id, false).await
}

async fn snapshot(State(st): State<AppState>, _v: Viewer, Path(id): Path<CameraId>) -> ApiResult<Response> {
    st.store.get_camera(id)?;
    let frame = st
        .engine
        .frames()
        .latest(id)
        .ok_or_else(|| ApiError::unavailable("no frame available yet"))?;
    Ok((
        [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")],
        frame.jpeg.clone(),
    )
        .into_response())
}

async fn live(State(st): State<AppState>, _v: Viewer, Path(id): Path<CameraId>) -> ApiResult<Response> {
    st.store.get_camera(id)?;
    Ok(stream::mjpeg_response(stream::live_stream(st.engine.frames(), id)))
}

async fn record_start(State(st): State<AppState>, _m: Manager, Path(id): Path<CameraId>) -> ApiResult<StatusCode> {
    st.store.get_camera(id)?;
    st.engine.set_manual_recording(id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn record_stop(State(st): State<AppState>, _m: Manager, Path(id): Path<CameraId>) -> ApiResult<StatusCode> {
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
