//! HTTP API (axum) + embedded web UI. See docs/SPEC.md §10–11.

pub mod api;
pub mod auth;
pub mod error;
pub mod stream;
pub mod web;

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use axum::Router;
use ss_core::Store;
use ss_engine::Engine;
use tokio::sync::watch;
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ApiResult};

/// How often open streams re-check their session (SPEC §10).
pub const SESSION_RECHECK: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub store: Arc<dyn Store>,
    /// Process start, for `uptime_seconds`.
    pub started: Instant,
    pub logins: Arc<auth::LoginLimiter>,
    /// Interval between session re-checks of open streams.
    pub session_recheck: Duration,
    /// Becomes `true` once shutdown is requested; open streams end then.
    pub shutdown: watch::Receiver<bool>,
    shutdown_tx: Arc<watch::Sender<bool>>,
}

impl AppState {
    pub fn new(engine: Engine, store: Arc<dyn Store>) -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            engine,
            store,
            started: Instant::now(),
            logins: Arc::new(auth::LoginLimiter::default()),
            session_recheck: SESSION_RECHECK,
            shutdown: rx,
            shutdown_tx: Arc::new(tx),
        }
    }

    /// Ask open streams to end (called on SIGINT/SIGTERM).
    pub fn request_shutdown(&self) {
        self.shutdown_tx.send_replace(true);
    }

    /// Resolves once [`AppState::request_shutdown`] has been called.
    pub fn shutdown_requested(&self) -> impl Future<Output = ()> + Send + 'static {
        stream::wait_true(self.shutdown.clone())
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
        // Log the path only: query strings can carry `?token=` session tokens.
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state)
}
