//! REST handlers grouped by resource. Each submodule owns its paths.

pub mod auth;
pub mod cameras;
pub mod events;
pub mod recordings;
pub mod system;
pub mod users;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::AppState;

/// `GET /health` (no prefix, no auth).
pub async fn health() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "ok").into_response()
}

/// All `/api/v1` routes (mounted by `crate::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(auth::routes())
        .merge(cameras::routes())
        .merge(recordings::routes())
        .merge(events::routes())
        .merge(users::routes())
        .merge(system::routes())
}
