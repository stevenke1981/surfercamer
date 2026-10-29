//! `/users` (admin only). Own password change lives in `api::auth`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use ss_core::{Role, User, UserId};

use crate::auth::{check_password_policy, hash_password_async, Admin};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{id}",
            axum::routing::put(update_user).delete(delete_user),
        )
}

const USERNAME_MAX: usize = 64;

fn admin_count(st: &AppState) -> ApiResult<usize> {
    Ok(st
        .store
        .list_users()?
        .iter()
        .filter(|u| u.role == Role::Admin)
        .count())
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

async fn create_user(
    State(st): State<AppState>,
    _a: Admin,
    ApiJson(req): ApiJson<NewUser>,
) -> ApiResult<(StatusCode, Json<User>)> {
    let username = req.username.trim();
    if username.is_empty() || username.len() > USERNAME_MAX {
        return Err(ApiError::invalid(format!(
            "username must be 1..={USERNAME_MAX} characters"
        )));
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
    Admin(me): Admin,
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
    let role_changed = req.role.is_some_and(|r| r != target.role);
    let password_changed = req.password.is_some();
    let hash = match req.password {
        Some(pw) => Some(hash_password_async(pw).await?),
        None => None,
    };
    let user = st.store.update_user(id, hash.as_deref(), req.role)?;
    if password_changed || role_changed {
        // Revoke the target's sessions. An admin editing their own account
        // keeps the session they are using.
        let keep = (id == me.user.id).then_some(me.token.as_str());
        st.store.delete_sessions_for_user(id, keep)?;
    }
    Ok(Json(user))
}

async fn delete_user(
    State(st): State<AppState>,
    Admin(me): Admin,
    Path(id): Path<UserId>,
) -> ApiResult<StatusCode> {
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
