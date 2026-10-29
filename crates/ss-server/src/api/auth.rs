//! `/auth/*`: login, logout, current user, own password change.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ss_core::User;

use crate::auth::{
    check_password_policy, clear_session_cookie, create_session, hash_password_async,
    session_cookie, verify_login_async, verify_password_async, CurrentUser,
};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/password", put(change_password))
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: User,
    pub expires_at: DateTime<Utc>,
}

async fn login(
    State(st): State<AppState>,
    ApiJson(req): ApiJson<LoginRequest>,
) -> ApiResult<Response> {
    let username = req.username.trim();
    // Reserve the attempt before any password work so parallel guesses are counted.
    let attempt = st.logins.begin(&username.to_lowercase()).ok_or_else(|| {
        ApiError::too_many_requests("too many failed logins; try again in a few minutes")
    })?;
    let user = match st.store.get_user_by_name(username) {
        Ok(u) => Some(u),
        Err(ss_core::Error::NotFound(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let phc = user.as_ref().map(|u| u.password_hash.clone());
    let ok = verify_login_async(req.password, phc).await?;
    let user = match user {
        Some(u) if ok => u,
        _ => {
            attempt.failed();
            return Err(ApiError::unauthorized("invalid username or password"));
        }
    };
    attempt.succeeded();
    let session = create_session(st.store.as_ref(), &user)?;
    let max_age = (session.expires_at - session.created_at).num_seconds();
    let cookie = session_cookie(&session.token, max_age);
    let body = LoginResponse {
        token: session.token,
        user,
        expires_at: session.expires_at,
    };
    Ok(([(header::SET_COOKIE, cookie)], Json(body)).into_response())
}

async fn logout(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Response> {
    match st.store.delete_session(&me.token) {
        Ok(()) | Err(ss_core::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok((
        [(header::SET_COOKIE, clear_session_cookie())],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

async fn me(me: CurrentUser) -> Json<User> {
    Json(me.user)
}

#[derive(Deserialize)]
pub struct PasswordChange {
    pub old_password: String,
    pub new_password: String,
}

async fn change_password(
    State(st): State<AppState>,
    me: CurrentUser,
    ApiJson(req): ApiJson<PasswordChange>,
) -> ApiResult<StatusCode> {
    if !verify_password_async(req.old_password.clone(), me.user.password_hash.clone()).await? {
        return Err(ApiError::unauthorized("old password is incorrect"));
    }
    check_password_policy(&req.new_password)?;
    let hash = hash_password_async(req.new_password).await?;
    st.store.update_user(me.user.id, Some(&hash), None)?;
    // Other sessions (possibly stolen ones) end; this one stays signed in.
    st.store
        .delete_sessions_for_user(me.user.id, Some(&me.token))?;
    Ok(StatusCode::NO_CONTENT)
}
