use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{random_hex, sha256_hex, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/auth/me", get(me))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
        .route("/auth/status", get(status))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
}

/// Public: tells the SPA whether it must show the first-run setup wizard.
async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "needs_setup": !state.auth_disabled && state.read().users.is_empty(),
        "auth_disabled": state.auth_disabled,
    }))
}

#[derive(Deserialize)]
struct SetupReq {
    username: String,
    password: String,
    #[serde(default)]
    nickname: String,
}

/// First-run admin creation. Refuses if any user already exists.
async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupReq>,
) -> Result<Response, ApiError> {
    if state.auth_disabled {
        return Err(ApiError::not_found("auth disabled"));
    }
    let username = req.username.trim().to_string();
    let password = req.password;
    if username.is_empty() || password.len() < 8 {
        return Err(ApiError::bad_request(
            "username required and password must be at least 8 characters",
        ));
    }

    let mut store = state.write();
    if !store.users.is_empty() {
        return Err(ApiError::conflict("already set up"));
    }

    let nickname = match req.nickname.trim() {
        "" => "admin".to_string(),
        n => n.to_string(),
    };
    let user = User::new(username, auth::hash_password(&password)?, nickname);
    store.acl.push(crate::perm::AclEntry {
        path: "/".into(),
        ugid: format!("user:{}", user.id),
        role: crate::perm::ADMIN_ROLE.into(),
        propagate: true,
    });
    store.users.push(user.clone());
    store.save(&state.data_dir)?;
    drop(store);

    login_response(&state, &user.username, &password)
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Response, ApiError> {
    if state.auth_disabled {
        return Err(ApiError::not_found("auth disabled"));
    }
    login_response(&state, req.username.trim(), &req.password)
}

fn login_response(state: &AppState, username: &str, password: &str) -> Result<Response, ApiError> {
    let user = {
        let store = state.read();
        store.user_by_username(username).cloned()
    }
    .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials"))?;

    if !auth::verify_password(password, &user.password_hash) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid credentials",
        ));
    }

    let token = random_hex(32);
    let mut store = state.write();
    store
        .sessions
        .insert(sha256_hex(token.as_bytes()), user.id.clone());
    store.save(&state.data_dir)?;
    drop(store);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::SET_COOKIE,
        auth::session_cookie(&token).parse().unwrap(),
    );
    let body = auth::me_json(&state.read(), &user);
    Ok((headers, Json(body)).into_response())
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(cookie) = headers.get(axum::http::header::COOKIE) {
        if let Ok(cookie) = cookie.to_str() {
            if let Some(token) = cookie
                .split(';')
                .map(str::trim)
                .filter_map(|c| c.split_once('='))
                .find(|(k, _)| *k == auth::SESSION_COOKIE)
                .map(|(_, v)| v)
            {
                let mut store = state.write();
                store.sessions.remove(&sha256_hex(token.as_bytes()));
                let _ = store.save(&state.data_dir);
            }
        }
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::SET_COOKIE,
        auth::cleared_cookie().parse().unwrap(),
    );
    (headers, Json(json!({"logged_out": true}))).into_response()
}

async fn me(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Json<Value> {
    Json(auth::me_json(&state.read(), &user))
}
