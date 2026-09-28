pub mod access;
pub mod activity;
pub mod admin;
pub mod auth;
pub mod connections;
pub mod health;
pub mod history;
pub mod keys;
pub mod profile;
pub mod proxy;
pub mod roles;
pub mod uploads;
pub mod users;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::AppState;
use crate::store::User;

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }

    pub fn bad_request(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn conflict(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn not_found(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

impl From<String> for ApiError {
    fn from(s: String) -> Self {
        ApiError::internal(s)
    }
}

/// Privilege check for handlers. Called after `require_user` has put the
/// user in request extensions.
pub fn enforce(state: &AppState, user: &User, path: &str, privilege: &str) -> Result<(), ApiError> {
    if state.read().can(user, path, privilege) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

/// Assemble the full router. State is provided to every module here, so the
/// returned router needs no further state.
pub fn build(state: AppState) -> axum::Router<AppState> {
    axum::Router::<AppState>::new()
        .merge(health::routes())
        .merge(auth::routes(&state))
        .merge(users::routes(&state))
        .merge(roles::routes(&state))
        .merge(profile::routes(&state))
        .merge(keys::routes(&state))
        .merge(activity::routes(&state))
        .merge(admin::routes(&state))
        .merge(access::routes(&state))
        .merge(history::routes(&state))
        .merge(uploads::routes(&state))
        .merge(connections::routes(&state))
        .merge(proxy::routes(&state))
}
