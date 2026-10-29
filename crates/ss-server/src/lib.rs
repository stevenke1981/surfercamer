//! HTTP API (axum) + embedded web UI. See docs/SPEC.md §10–11.

pub mod api;
pub mod auth;
pub mod error;
pub mod stream;
pub mod web;

use std::sync::Arc;
use std::time::Instant;

use axum::routing::get;
use axum::Router;
use ss_core::Store;
use ss_engine::Engine;
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ApiResult};

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub store: Arc<dyn Store>,
    /// Process start, for `uptime_seconds`.
    pub started: Instant,
    pub logins: Arc<auth::LoginLimiter>,
}

impl AppState {
    pub fn new(engine: Engine, store: Arc<dyn Store>) -> Self {
        Self { engine, store, started: Instant::now(), logins: Arc::new(auth::LoginLimiter::default()) }
    }
}

/// Full application router (`/api/v1/*` + web UI at `/`).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route("/app.js", get(web::app_js))
        .route("/style.css", get(web::style_css))
        .route("/health", get(api::health))
        .nest("/api/v1", api::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
