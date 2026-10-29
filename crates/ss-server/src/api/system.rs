//! `/settings` (admin) and `/system/info` (viewer).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use ss_core::{ConnectionState, Settings};

use crate::auth::{Admin, Viewer};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).put(put_settings))
        .route("/system/info", get(system_info))
}

async fn get_settings(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Settings>> {
    Ok(Json(st.store.get_settings()?))
}

async fn put_settings(
    State(st): State<AppState>,
    _a: Admin,
    ApiJson(s): ApiJson<Settings>,
) -> ApiResult<Json<Settings>> {
    if !(1..=720).contains(&s.session_ttl_hours) {
        return Err(ApiError::invalid("session_ttl_hours must be 1..=720"));
    }
    if s.retention_interval_seconds < 10 {
        return Err(ApiError::invalid(
            "retention_interval_seconds must be >= 10",
        ));
    }
    st.store.put_settings(&s)?;
    Ok(Json(s))
}

async fn system_info(State(st): State<AppState>, _v: Viewer) -> ApiResult<Json<Value>> {
    let cameras = st.store.list_cameras()?;
    let statuses = st.engine.statuses();
    let online = statuses
        .iter()
        .filter(|s| s.state == ConnectionState::Online)
        .count();
    let recording = statuses.iter().filter(|s| s.recording).count();
    let used_bytes = st.store.recording_usage(None)?;
    let root = st.engine.storage_root().to_path_buf();
    let (disk, ffmpeg) = tokio::task::spawn_blocking(move || {
        let disk = ss_recorder::disk::disk_space(&root);
        (disk, ss_camera::rtsp::ffmpeg_available())
    })
    .await?;
    let (total_bytes, free_bytes) = match disk {
        Ok(d) => (d.total_bytes, d.free_bytes),
        Err(e) => {
            tracing::warn!(error = %e, "disk_space failed");
            (0, 0)
        }
    };
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": st.started.elapsed().as_secs(),
        "cameras": {
            "total": cameras.len(),
            "online": online,
            "recording": recording,
        },
        "storage": {
            "root": st.engine.storage_root().display().to_string(),
            "used_bytes": used_bytes,
            "total_bytes": total_bytes,
            "free_bytes": free_bytes,
        },
        "ffmpeg": ffmpeg,
    })))
}
