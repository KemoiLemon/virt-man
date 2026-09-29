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

/// The privilege and path a series needs, matching the proxy's mapping of
/// the daemon routes the same numbers come from live. Datacenter totals span
/// every daemon, so they are an administrator's view.
fn need(q: &history::Query) -> (String, &'static str) {
    match q.kind.as_str() {
        "vm" | "nic" | "disk" if q.ct => (crate::perm::ct_path(&q.conn, &q.name), "CT.Audit"),
        "vm" | "nic" | "disk" => (crate::perm::vm_path(&q.conn, &q.name), "VM.Audit"),
        "host" => (crate::perm::node_path(&q.conn), "Sys.Audit"),
        _ => ("/".into(), "Permissions.Modify"),
    }
}

async fn series(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<history::Query>,
) -> Result<Json<Value>, ApiError> {
    let (path, privilege) = need(&q);
    super::enforce(&state, &user, &path, privilege)?;
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
    fn q(kind: &str) -> crate::history::Query {
        serde_json::from_value(serde_json::json!({
            "conn": "c", "kind": kind, "name": "web", "since_ms": 0
        }))
        .unwrap()
    }

    #[test]
    fn guest_series_need_that_guest() {
        for k in ["vm", "nic", "disk"] {
            assert_eq!(super::need(&q(k)), ("/nodes/c/vms/web".into(), "VM.Audit"));
        }
        assert_eq!(super::need(&q("host")), ("/nodes/c".into(), "Sys.Audit"));
        assert_eq!(super::need(&q("dc")).1, "Permissions.Modify");
    }
}
