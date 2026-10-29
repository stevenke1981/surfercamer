//! `/users` (admin only). Own password change lives in `api::auth`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use ss_core::{Role, User, UserId};

use crate::auth::{check_password_policy, hash_password_async, Admin, CurrentUser};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", axum::routing::put(update_user).delete(delete_user))
}

const USERNAME_MAX: usize = 64;

fn admin_count(st: &AppState) -> ApiResult<usize> {
    Ok(st.store.list_users()?.iter().filter(|u| u.role == Role::Admin).count())
}

async fn list_users(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Vec<User>>> {
    Ok(Json(st.store.list_users()?))
}

#[derive(Deserialize)]
pub struct NewUser {
    pub username: String,
    pub password: String,
    pub role: Role,
}

async fn create_user(State(st): State<AppState>, _a: Admin, ApiJson(req): ApiJson<NewUser>) -> ApiResult<(StatusCode, Json<User>)> {
    let username = req.username.trim();
    if username.is_empty() || username.len() > USERNAME_MAX {
        return Err(ApiError::invalid(format!("username must be 1..={USERNAME_MAX} characters")));
    }
    check_password_policy(&req.password)?;
    let hash = hash_password_async(req.password).await?;
    let user = st.store.create_user(username, &hash, req.role)?;
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Deserialize)]
pub struct UserUpdate {
    pub password: Option<String>,
    pub role: Option<Role>,
}

async fn update_user(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<UserId>,
    ApiJson(req): ApiJson<UserUpdate>,
) -> ApiResult<Json<User>> {
    let target = st.store.get_user(id)?;
    if let Some(pw) = &req.password {
        check_password_policy(pw)?;
    }
    if let Some(new_role) = req.role {
        if target.role == Role::Admin && new_role != Role::Admin && admin_count(&st)? <= 1 {
            return Err(ApiError::conflict("cannot demote the last admin"));
        }
    }
    let hash = match req.password {
        Some(pw) => Some(hash_password_async(pw).await?),
        None => None,
    };
    Ok(Json(st.store.update_user(id, hash.as_deref(), req.role)?))
}

async fn delete_user(State(st): State<AppState>, me: CurrentUser, Path(id): Path<UserId>) -> ApiResult<StatusCode> {
    let target = st.store.get_user(id)?;
    if me.user.id == id {
        return Err(ApiError::conflict("cannot delete your own account"));
    }
    if target.role == Role::Admin && admin_count(&st)? <= 1 {
        return Err(ApiError::conflict("cannot delete the last admin"));
    }
    st.store.delete_user(id)?;
    Ok(StatusCode::NO_CONTENT)
}
