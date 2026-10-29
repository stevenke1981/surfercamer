//! HTTP error type and request extractors that map rejections to JSON errors.
//! See docs/SPEC.md §4.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::AppState;

pub type ApiResult<T> = Result<T, ApiError>;

/// An error rendered as `{"error": <code>, "message": <text>}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "insufficient role for this operation",
        )
    }

    /// Cookie-authenticated unsafe request without `X-Requested-With: ss`.
    pub fn csrf_required() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "missing X-Requested-With header for cookie-authenticated request",
        )
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// Generic 500; the real cause is logged, never returned.
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        tracing::error!(error = %cause, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal server error",
        )
    }
}

impl From<ss_core::Error> for ApiError {
    fn from(err: ss_core::Error) -> Self {
        use ss_core::Error as E;
        match err {
            E::NotFound(m) => Self::not_found(m),
            E::Invalid(m) => Self::invalid(m),
            E::Conflict(m) => Self::conflict(m),
            E::Unauthorized => Self::unauthorized("authentication required"),
            E::Forbidden => Self::forbidden(),
            E::Camera(m) => Self::new(StatusCode::BAD_GATEWAY, "camera", m),
            other => Self::internal(other),
        }
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(err: tokio::task::JoinError) -> Self {
        Self::internal(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.code, "message": self.message });
        (self.status, Json(body)).into_response()
    }
}

/// JSON body extractor whose rejections are `invalid` API errors.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ApiError::invalid(rejection.body_text())),
        }
    }
}

/// Query-string extractor whose rejections are `invalid` API errors.
pub struct ApiQuery<T>(pub T);

impl<T> FromRequestParts<AppState> for ApiQuery<T>
where
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(ApiError::invalid(rejection.to_string())),
        }
    }
}
