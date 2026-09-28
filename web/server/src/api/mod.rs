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
    let store = state.read();
    if store.can(user, path, privilege) {
        Ok(())
    } else {
        Err(denied(&store, user, path, privilege))
    }
}

/// A refusal that says what was missing, on what, and why -- "forbidden" left
/// users guessing whether they lacked a permission, hit a limit, or found a
/// bug. Suspension is named, since it looks like a revoked grant otherwise.
pub fn denied(store: &crate::store::Store, user: &User, path: &str, privilege: &str) -> ApiError {
    let server = |c: &str| store.connection(c).map(|x| x.name.clone());
    let what = crate::perm::describe_path(path, &server);
    let suspended = crate::perm::path_resource(path)
        .and_then(|(k, c, n)| store.resource(k, c, n))
        .is_some_and(|r| r.suspended);
    let msg = if suspended
        && store
            .privs(user, path)
            .iter()
            .any(|p| p.ends_with(".Audit"))
    {
        format!("{what} is suspended by an administrator; it is read-only until they restore it")
    } else {
        format!(
            "you do not have permission to {} ({what}; needs {privilege}). Ask an administrator to grant it.",
            crate::perm::describe_priv(privilege)
        )
    };
    ApiError::new(StatusCode::FORBIDDEN, msg)
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
