use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use super::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{avatar_path, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    // Protected profile routes, then the public avatar-serving route added
    // after the auth layer so it stays unauthenticated.
    Router::new()
        .route("/me", axum::routing::put(update_me))
        .route("/me/password", post(change_password))
        .route("/me/avatar", post(upload_avatar))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
        .route("/avatars/{file}", get(serve_avatar))
}

#[derive(Deserialize)]
struct UpdateMe {
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

async fn update_me(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<UpdateMe>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(idx) = store.users.iter().position(|u| u.id == user.id) else {
        return Err(ApiError::not_found("user"));
    };

    if let Some(ref username) = req.username {
        let username = username.trim();
        if username.is_empty() {
            return Err(ApiError::bad_request("username is required"));
        }
        if store
            .users
            .iter()
            .any(|u| u.id != user.id && u.username == username)
        {
            return Err(ApiError::conflict("username already exists"));
        }
    }

    let me = &mut store.users[idx];
    if let Some(username) = req.username {
        me.username = username.trim().to_string();
    }
    if let Some(nickname) = req.nickname {
        let nickname = nickname.trim();
        if nickname.is_empty() {
            return Err(ApiError::bad_request("nickname is required"));
        }
        me.nickname = nickname.to_string();
    }
    let me = me.clone();
    store.save(&state.data_dir)?;
    // The SPA replaces its identity with this answer, so it must carry what
    // /auth/me does, or saving a profile hides the admin button.
    Ok(Json(auth::me_json(&store, &me)))
}

#[derive(Deserialize)]
struct PasswordReq {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<PasswordReq>,
) -> Result<Json<Value>, ApiError> {
    if req.new.len() < 8 {
        return Err(ApiError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    let mut store = state.write();
    let Some(me) = store.users.iter_mut().find(|u| u.id == user.id) else {
        return Err(ApiError::not_found("user"));
    };
    if !auth::verify_password(&req.current, &me.password_hash) {
        return Err(ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "current password is incorrect",
        ));
    }
    me.password_hash = auth::hash_password(&req.new)?;
    let me = me.clone();
    store.save(&state.data_dir)?;
    // The SPA replaces its identity with this answer, so it must carry what
    // /auth/me does, or saving a profile hides the admin button.
    Ok(Json(auth::me_json(&store, &me)))
}

/// Accept raw image bytes; `?ext=` selects the stored extension.
async fn upload_avatar(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let ext = q
        .get("ext")
        .map(String::as_str)
        .unwrap_or("png")
        .to_ascii_lowercase();
    if !["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
        return Err(ApiError::bad_request(
            "avatar must be png, jpg, jpeg or webp",
        ));
    }
    if body.is_empty() || body.len() > 2 * 1024 * 1024 {
        return Err(ApiError::bad_request("avatar must be 1 B .. 2 MB"));
    }

    let mut store = state.write();
    let seq = store.avatar_seq;
    store.avatar_seq += 1;
    let Some(u) = store.users.iter_mut().find(|u| u.id == user.id) else {
        return Err(ApiError::not_found("user"));
    };
    let path = avatar_path(&state.data_dir, seq, &ext);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(ApiError::internal)?;
    std::fs::write(&path, &body).map_err(ApiError::internal)?;
    u.avatar = Some(path.file_name().unwrap().to_string_lossy().into_owned());
    let user = u.clone();
    store.save(&state.data_dir)?;
    Ok(Json(auth::me_json(&store, &user)))
}

async fn serve_avatar(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    // Never allow path traversal out of the avatars dir.
    if file.contains('/') || file.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = state.data_dir.join("avatars").join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ct = match file.rsplit('.').next().unwrap_or("") {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
