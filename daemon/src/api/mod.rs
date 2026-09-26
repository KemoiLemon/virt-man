pub mod console;
pub mod domains;
pub mod filters;
pub mod health;
pub mod host;
pub mod networks;
pub mod pools;
pub mod volumes;

use axum::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::auth::Auth;

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }

    pub fn bad_request(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

pub fn build(auth: Auth) -> Router {
    let authed = Router::new()
        .merge(health::routes())
        .merge(host::routes())
        .merge(domains::routes())
        .merge(networks::routes())
        .merge(filters::routes())
        .merge(pools::routes())
        .merge(volumes::routes())
        .layer(middleware::from_fn(crate::auth::require_token));

    // console authenticates via ?token= (browsers can't set WS headers), so it
    // is merged after the middleware layer. Auth is shared via Extension.
    Router::new()
        .merge(authed)
        .merge(console::routes())
        .layer(Extension(auth))
}
