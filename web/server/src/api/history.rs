//! `GET /history`: a sampled series over a time range, for the range charts.
//! Readable by anyone who can read the subject it describes.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{middleware, Json, Router};
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::history;
use crate::state::AppState;
use crate::store::User;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/history", get(series))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

/// Permission scope for a series kind, matching the proxy's mapping of the
/// daemon routes the same numbers come from live.
fn scope(kind: &str) -> &'static str {
    match kind {
        "vm" | "nic" | "disk" => "vms",
        _ => "connections",
    }
}

async fn series(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<history::Query>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, scope(&q.kind), false)?;
    let h = state.history.clone();
    let now = history::now_ms() as i64;
    let (rows, step) = tokio::task::spawn_blocking(move || h.query(&q, now))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({
        "points": rows,
        "step_ms": step,
        "now_ms": now,
        "sample_ms": history::SAMPLE_MS,
    })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn guest_series_need_vm_access() {
        assert_eq!(super::scope("vm"), "vms");
        assert_eq!(super::scope("nic"), "vms");
        assert_eq!(super::scope("disk"), "vms");
        assert_eq!(super::scope("host"), "connections");
        assert_eq!(super::scope("dc"), "connections");
    }
}
