//! `GET /access-logs`: guest connections, filtered, grouped and sorted.
//! Needs read access to VMs, like the rest of what guests do.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{middleware, Json, Router};
use serde_json::{json, Value};

use super::ApiError;
use crate::access;
use crate::auth;
use crate::state::AppState;
use crate::store::User;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/access-logs", get(logs))
        .route("/access-logs/vms", get(vms))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

async fn logs(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<access::Query>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "vms", false)?;
    if state.read().connection(&q.conn).is_none() {
        return Err(ApiError::not_found("no such connection"));
    }
    let a = state.access.clone();
    let now = crate::history::now_ms() as i64;
    let mut v = tokio::task::spawn_blocking(move || a.query(&q, now))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    v["retain_ms"] = json!(access::RETAIN_MS);
    Ok(Json(v))
}

#[derive(serde::Deserialize)]
struct VmsQuery {
    conn: String,
}

/// VM names with logged traffic, for the filter.
async fn vms(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<VmsQuery>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "vms", false)?;
    let a = state.access.clone();
    let names = tokio::task::spawn_blocking(move || a.vms(&q.conn))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "vms": names })))
}
