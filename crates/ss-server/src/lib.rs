//! HTTP API (axum) + embedded web UI. See docs/SPEC.md §10–11.

use std::sync::Arc;

use axum::Router;
use ss_core::Store;
use ss_engine::Engine;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub store: Arc<dyn Store>,
}

/// Full application router (`/api/v1/*` + web UI at `/`).
pub fn router(state: AppState) -> Router {
    let _ = state;
    todo!()
}
