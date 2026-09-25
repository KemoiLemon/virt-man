use axum::extract::{Path, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{random_hex, Role, Scopes, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{id}", axum::routing::put(update).delete(remove))
        .layer(middleware::from_fn(auth::require_scope(
            state.clone(),
            "users",
            true,
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let users: Vec<Value> = state.read().users.iter().map(auth::user_json).collect();
    Ok(Json(json!(users)))
}

#[derive(Deserialize)]
struct CreateUser {
    username: String,
    password: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    role: Role,
    #[serde(default)]
    scopes: Scopes,
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateUser>,
) -> Result<Json<Value>, ApiError> {
    let username = req.username.trim().to_string();
    if username.is_empty() || req.password.len() < 8 {
        return Err(ApiError::bad_request(
            "username required and password must be at least 8 characters",
        ));
    }
    let mut store = state.write();
    if store.user_by_username(&username).is_some() {
        return Err(ApiError::conflict("username already exists"));
    }
    let user = User {
        id: random_hex(8),
        username,
        password_hash: auth::hash_password(&req.password)?,
        role: req.role,
        nickname: if req.nickname.trim().is_empty() {
            "user".to_string()
        } else {
            req.nickname.trim().to_string()
        },
        avatar: None,
        scopes: req.scopes,
    };
    store.users.push(user.clone());
    store.save(&state.data_dir)?;
    Ok(Json(auth::user_json(&user)))
}

#[derive(Deserialize)]
struct UpdateUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    scopes: Option<Scopes>,
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(actor): axum::extract::Extension<User>,
    Json(req): Json<UpdateUser>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(idx) = store.users.iter().position(|u| u.id == id) else {
        return Err(ApiError::not_found("no such user"));
    };

    // Validate against other users before taking the mutable borrow.
    if let Some(ref username) = req.username {
        let username = username.trim();
        if username.is_empty() {
            return Err(ApiError::bad_request("username required"));
        }
        if store
            .users
            .iter()
            .any(|u| u.id != id && u.username == username)
        {
            return Err(ApiError::conflict("username already exists"));
        }
    }
    if let Some(role) = req.role {
        let last_admin = store.users[idx].id == actor.id
            && store.users[idx].role == Role::Admin
            && role != Role::Admin
            && store.users.iter().filter(|u| u.role == Role::Admin).count() <= 1;
        if last_admin {
            return Err(ApiError::bad_request("cannot demote the last admin"));
        }
    }

    let user = &mut store.users[idx];
    if let Some(username) = req.username {
        user.username = username.trim().to_string();
    }
    if let Some(password) = req.password {
        if password.len() < 8 {
            return Err(ApiError::bad_request(
                "password must be at least 8 characters",
            ));
        }
        user.password_hash = auth::hash_password(&password)?;
    }
    if let Some(nickname) = req.nickname {
        user.nickname = if nickname.trim().is_empty() {
            "user".into()
        } else {
            nickname.trim().into()
        };
    }
    if let Some(role) = req.role {
        user.role = role;
    }
    if let Some(scopes) = req.scopes {
        user.scopes = scopes;
    }
    let user = user.clone();
    store.save(&state.data_dir)?;
    Ok(Json(auth::user_json(&user)))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(actor): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    if actor.id == id {
        return Err(ApiError::bad_request("cannot delete your own account"));
    }
    let mut store = state.write();
    let Some(pos) = store.users.iter().position(|u| u.id == id) else {
        return Err(ApiError::not_found("no such user"));
    };
    let role = store.users[pos].role;
    if role == Role::Admin {
        let admins = store.users.iter().filter(|u| u.role == Role::Admin).count();
        if admins <= 1 {
            return Err(ApiError::bad_request("cannot delete the last admin"));
        }
    }
    store.users.remove(pos);
    store.api_keys.retain(|k| k.user_id != id);
    store.sessions.retain(|_, uid| uid != &id);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}
