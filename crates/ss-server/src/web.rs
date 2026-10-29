//! Embedded single-page web UI (no build step, no external CDNs). SPEC §11.

use axum::http::header;
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

pub async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

pub async fn app_js() -> Response {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], APP_JS).into_response()
}

pub async fn style_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE_CSS).into_response()
}
