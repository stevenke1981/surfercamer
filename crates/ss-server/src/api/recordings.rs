//! `/recordings/*` and `/timeline`.

use std::path::{Component, Path as FsPath, PathBuf};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use ss_core::{CameraId, Event, EventQuery, Recording, RecordingId, RecordingQuery};
use ss_recorder::segment::SegmentReader;
use tokio::io::AsyncReadExt;

use crate::auth::{Manager, Viewer};
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::stream;
use crate::AppState;

const PAGE_MAX: u32 = 1000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/recordings", get(list_recordings))
        .route(
            "/recordings/{id}",
            get(get_recording).delete(delete_recording),
        )
        .route("/recordings/{id}/lock", post(lock_recording))
        .route("/recordings/{id}/unlock", post(unlock_recording))
        .route("/recordings/{id}/stream", get(stream_recording))
        .route("/recordings/{id}/frame", get(frame_at))
        .route("/recordings/{id}/download", get(download_recording))
        .route("/timeline", get(timeline))
}

async fn list_recordings(
    State(st): State<AppState>,
    _v: Viewer,
    ApiQuery(q): ApiQuery<RecordingQuery>,
) -> ApiResult<Json<Vec<Recording>>> {
    Ok(Json(st.store.list_recordings(&q)?))
}

async fn get_recording(
    State(st): State<AppState>,
    _v: Viewer,
    Path(id): Path<RecordingId>,
) -> ApiResult<Json<Recording>> {
    Ok(Json(st.store.get_recording(id)?))
}

/// Segment files live under the storage root; reject anything that could escape it.
fn checked_rel_path(rel: &str) -> ApiResult<PathBuf> {
    let p = FsPath::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(ApiError::internal(format!("unsafe recording path {rel:?}")));
    }
    Ok(p.to_path_buf())
}

async fn delete_recording(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<RecordingId>,
) -> ApiResult<StatusCode> {
    let rec = st.store.get_recording(id)?;
    if rec.locked {
        return Err(ApiError::conflict("recording is locked"));
    }
    if rec.end.is_none() {
        return Err(ApiError::conflict("recording is still in progress"));
    }
    let root = st.engine.storage_root().to_path_buf();
    let rel = rec.path.clone();
    tokio::task::spawn_blocking(move || ss_recorder::retention::delete_segment_files(&root, &rel))
        .await??;
    st.store.delete_recording(id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn lock_recording(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<RecordingId>,
) -> ApiResult<Json<Recording>> {
    Ok(Json(st.store.set_recording_locked(id, true)?))
}

async fn unlock_recording(
    State(st): State<AppState>,
    _m: Manager,
    Path(id): Path<RecordingId>,
) -> ApiResult<Json<Recording>> {
    Ok(Json(st.store.set_recording_locked(id, false)?))
}

#[derive(Deserialize)]
pub struct PlaybackQuery {
    pub offset_ms: Option<u32>,
    pub speed: Option<f64>,
}

fn clamp_speed(speed: Option<f64>) -> f64 {
    match speed {
        Some(s) if s.is_finite() => s.clamp(0.25, 16.0),
        _ => 1.0,
    }
}

async fn open_segment(st: &AppState, rec: &Recording) -> ApiResult<SegmentReader> {
    let root = st.engine.storage_root().to_path_buf();
    let rel = checked_rel_path(&rec.path)?.to_string_lossy().into_owned();
    let reader = tokio::task::spawn_blocking(move || SegmentReader::open(&root, &rel)).await??;
    Ok(reader)
}

async fn stream_recording(
    State(st): State<AppState>,
    Viewer(me): Viewer,
    Path(id): Path<RecordingId>,
    ApiQuery(q): ApiQuery<PlaybackQuery>,
) -> ApiResult<Response> {
    let rec = st.store.get_recording(id)?;
    let reader = open_segment(&st, &rec).await?;
    let speed = clamp_speed(q.speed);
    let start = reader
        .index_at(q.offset_ms.unwrap_or(0))
        .unwrap_or(reader.entries().len());
    let frames = stream::playback_stream(reader, start, speed);
    Ok(stream::mjpeg_response(stream::bounded(
        &st, me.token, frames,
    )))
}

async fn frame_at(
    State(st): State<AppState>,
    _v: Viewer,
    Path(id): Path<RecordingId>,
    ApiQuery(q): ApiQuery<PlaybackQuery>,
) -> ApiResult<Response> {
    let rec = st.store.get_recording(id)?;
    let mut reader = open_segment(&st, &rec).await?;
    let offset = q.offset_ms.unwrap_or(0);
    let jpeg = tokio::task::spawn_blocking(move || -> ss_core::Result<bytes::Bytes> {
        let idx = reader
            .index_at(offset)
            .ok_or_else(|| ss_core::Error::NotFound("no frames in recording".into()))?;
        reader.read_frame(idx)
    })
    .await??;
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        jpeg,
    )
        .into_response())
}

async fn download_recording(
    State(st): State<AppState>,
    Viewer(me): Viewer,
    Path(id): Path<RecordingId>,
) -> ApiResult<Response> {
    let rec = st.store.get_recording(id)?;
    let rel = checked_rel_path(&rec.path)?;
    let path = st.engine.storage_root().join(&rel);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ApiError::not_found("recording file is missing"),
            _ => ApiError::internal(e),
        })?;
    let raw = futures_util::stream::unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Vec<u8>, std::io::Error>(buf), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    let chunks = stream::bounded(&st, me.token, raw);
    let filename = format!("recording-{}.mjpg", rec.id);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from_stream(chunks),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    pub camera_id: CameraId,
    /// Local calendar day, `YYYY-MM-DD`.
    pub date: String,
}

#[derive(Serialize)]
pub struct Timeline {
    pub recordings: Vec<Recording>,
    pub events: Vec<Event>,
}

/// Start of a local calendar day, as UTC.
fn local_midnight(day: NaiveDate) -> ApiResult<DateTime<Utc>> {
    let naive = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| ApiError::invalid("invalid date"))?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| ApiError::invalid("date has no valid local midnight"))
}

async fn timeline(
    State(st): State<AppState>,
    _v: Viewer,
    ApiQuery(q): ApiQuery<TimelineQuery>,
) -> ApiResult<Json<Timeline>> {
    let day = NaiveDate::parse_from_str(&q.date, "%Y-%m-%d")
        .map_err(|_| ApiError::invalid("date must be YYYY-MM-DD"))?;
    let next = day
        .succ_opt()
        .ok_or_else(|| ApiError::invalid("date out of range"))?;
    let from = local_midnight(day)?;
    let to = local_midnight(next)?;
    st.store.get_camera(q.camera_id)?;
    let recordings = st.store.list_recordings(&RecordingQuery {
        camera_id: Some(q.camera_id),
        from: Some(from),
        to: Some(to),
        limit: Some(PAGE_MAX),
        ..Default::default()
    })?;
    let events = st.store.list_events(&EventQuery {
        camera_id: Some(q.camera_id),
        from: Some(from),
        to: Some(to),
        limit: Some(PAGE_MAX),
        ..Default::default()
    })?;
    Ok(Json(Timeline { recordings, events }))
}
