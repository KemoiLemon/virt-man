use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::middleware;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;

use super::ApiError;
use crate::auth;
use crate::daemon;
use crate::state::AppState;
use crate::store::User;

pub fn routes(state: &AppState) -> Router<AppState> {
    // The console is plain HTTP now (the RFB client lives in the daemon and
    // frames are fetched), so it needs no route of its own.
    Router::new()
        .route("/proxy/{conn_id}/{*rest}", any(proxy))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

/// Map a proxied daemon path to the permission scope it belongs to.
fn path_scope(path: &str) -> &'static str {
    if path.contains("/domains") {
        "vms"
    } else if path.contains("/networks") {
        "networks"
    } else if path.contains("/pools") {
        "storage"
    } else {
        "connections"
    }
}

/// Human-readable label for a destructive proxied call, or `None` when the
/// path is not one we record (so an unrecognised route adds no log noise).
///
/// Names are used as they appear in the path. They are percent-encoded, which
/// is unambiguous and safe to store; decoding them for cosmetics is not worth
/// a second encoder to keep in step.
fn delete_label(path: &str) -> Option<String> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match seg.as_slice() {
        ["domains", name] => Some(format!("Delete VM {name}")),
        ["filters", name] => Some(format!("Delete filter {name}")),
        ["networks", name] => Some(format!("Delete network {name}")),
        ["pools", name] => Some(format!("Delete storage pool {name}")),
        // A volume's name means nothing without its pool, so both are recorded.
        ["pools", pool, "volumes", vol] => Some(format!("Delete volume {vol} from pool {pool}")),
        _ => None,
    }
}

/// Summarise what a VM deletion did with its storage.
///
/// Worth recording: this is the field that says whether a shared disk or an
/// ISO was deliberately spared, which is exactly the question later.
fn summarize_cleanup(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return String::new();
    };
    let count = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0)
    };
    let kept = v
        .get("kept")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut reasons: Vec<String> = kept
        .iter()
        .filter_map(|k| k.get("reason").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    reasons.sort();
    reasons.dedup();

    let removed = count("removed");
    let failed = count("failed");
    let mut parts = vec![format!("removed {removed} disk(s)")];
    if !kept.is_empty() {
        let why = if reasons.is_empty() {
            "kept".to_string()
        } else {
            reasons.join(", ")
        };
        parts.push(format!("kept {} ({why})", kept.len()));
    }
    if failed > 0 {
        parts.push(format!("{failed} could not be removed"));
    }
    parts.join("; ")
}

/// Describe a destructive proxied call for the activity log: the label, the
/// outcome, and any detail worth keeping. `None` when the path is not a
/// deletion we track.
///
/// Pure so every mapping and message is testable without a request — this is
/// what answers "who deleted this, and what happened", so it should not be
/// reachable only through a live proxied delete.
fn delete_activity(
    path_and_query: &str,
    status: u16,
    body: &str,
) -> Option<(String, &'static str, String)> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };
    let mut label = delete_label(path)?;
    // Deleting with storage is a materially different action from deleting the
    // definition, so the label distinguishes them.
    let with_storage = query.split('&').any(|kv| kv == "remove_storage=true");
    if with_storage {
        label.push_str(" (with storage)");
    }

    if !(200..300).contains(&status) {
        // The daemon's own wording is the most useful thing to keep; fall back
        // to the raw body so a non-JSON error is not silently dropped.
        let msg = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(200).collect());
        return Some((label, "failed", msg));
    }

    let message = if with_storage {
        summarize_cleanup(body)
    } else {
        String::new()
    };
    Some((label, "done", message))
}

/// Record a destructive call, best effort.
///
/// A failure to log must never turn a completed deletion into a failed one,
/// so this only warns — the same rule the upload pipeline follows.
fn record_delete(state: &AppState, user: &User, path_and_query: &str, status: u16, body: &str) {
    let Some((label, outcome, message)) = delete_activity(path_and_query, status, body) else {
        return;
    };
    if let Err(e) =
        super::activity::append(state, &user.id, &user.username, &label, outcome, &message)
    {
        tracing::warn!("could not record deletion activity: {e}");
    }
}

/// All daemon calls proxy through here. The daemon token never reaches the
/// browser: the backend attaches it server-side.
async fn proxy(
    State(state): State<AppState>,
    Path((conn_id, _rest)): Path<(String, String)>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let write = method != axum::http::Method::GET && method != axum::http::Method::HEAD;
    // Captured before `method` is moved into the request below.
    let destructive = method == axum::http::Method::DELETE;
    super::enforce(&state, &user, path_scope(uri.path()), write)?;

    let conn = {
        let store = state.read();
        let Some(c) = store.connection(&conn_id) else {
            return Err(ApiError::not_found("no such connection"));
        };
        c.clone()
    };

    let base = daemon::parse_url(&conn.url)?.base;
    // uri comes in as /proxy/{id}/rest/of/path — strip the prefix.
    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let rest = path_and_query
        .strip_prefix(&format!("/proxy/{conn_id}"))
        .unwrap_or(path_and_query);
    let target = format!("{}{}", base, rest);

    let client =
        daemon::http_client(&conn).map_err(|e| ApiError::internal(format!("client: {e}")))?;
    let mut builder = client
        .request(method, &target)
        .header(header::AUTHORIZATION, format!("Bearer {}", conn.token));

    for (name, value) in headers.iter() {
        if matches!(name.as_str(), "content-type" | "accept" | "user-agent") {
            builder = builder.header(name, value);
        }
    }

    // Stream the request body straight through (ISO uploads are large).
    let stream = body
        .into_data_stream()
        .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    let resp = builder
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|e| {
            let mut msg = format!("daemon error: {e}");
            let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(" | {s}"));
                src = s.source();
            }
            ApiError::bad_request(msg)
        })?;

    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::internal(format!("read daemon body: {e}")))?;

    // Recorded in the backend rather than the SPA: this way every client is
    // covered, including an API key or a direct call, not just our buttons.
    if destructive {
        record_delete(
            &state,
            &user,
            rest,
            status.as_u16(),
            &String::from_utf8_lossy(&bytes),
        );
    }

    let mut out = Response::builder().status(status);
    for (name, value) in resp_headers.iter() {
        if name.as_str() != "transfer-encoding" && name.as_str() != "connection" {
            out = out.header(name, value);
        }
    }
    out.body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("build response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{delete_activity, delete_label, summarize_cleanup};

    #[test]
    fn every_destructive_route_gets_a_named_label() {
        assert_eq!(delete_label("/domains/web1").unwrap(), "Delete VM web1");
        assert_eq!(delete_label("/networks/lan").unwrap(), "Delete network lan");
        assert_eq!(
            delete_label("/pools/default").unwrap(),
            "Delete storage pool default"
        );
        assert_eq!(
            delete_label("/pools/ISOs/volumes/debian.iso").unwrap(),
            "Delete volume debian.iso from pool ISOs"
        );
        assert_eq!(
            delete_label("/filters/clean-traffic").unwrap(),
            "Delete filter clean-traffic"
        );
    }

    /// Non-destructive paths must not be logged, or the log fills with noise
    /// and stops being usable for the question it exists to answer.
    #[test]
    fn non_deletions_are_not_labelled() {
        for p in [
            "/domains",
            "/host/stats",
            "/domains/web1/start",
            "/domains/web1/xml",
            "/pools/ISOs/volumes",
            "",
        ] {
            assert!(delete_label(p).is_none(), "unexpectedly labelled: {p}");
        }
    }

    #[test]
    fn deleting_with_storage_is_distinguished_from_deleting_the_definition() {
        let (plain, _, _) = delete_activity("/domains/web1", 200, "{}").unwrap();
        assert_eq!(plain, "Delete VM web1");
        let (with, _, _) = delete_activity("/domains/web1?remove_storage=true", 200, "{}").unwrap();
        assert_eq!(with, "Delete VM web1 (with storage)");
        // Order within the query must not matter.
        let (mixed, _, _) =
            delete_activity("/domains/web1?force=1&remove_storage=true", 200, "{}").unwrap();
        assert_eq!(mixed, "Delete VM web1 (with storage)");
    }

    #[test]
    fn a_successful_delete_is_recorded_as_done() {
        let (label, status, _) = delete_activity("/networks/lan", 200, "").unwrap();
        assert_eq!(label, "Delete network lan");
        assert_eq!(status, "done");
    }

    /// A failed deletion is exactly what someone will later need to find, so it
    /// must be recorded with the daemon's reason.
    #[test]
    fn a_refused_delete_keeps_the_daemons_reason() {
        let body = r#"{"error":"volume is in use by web1"}"#;
        let (_, status, message) =
            delete_activity("/pools/default/volumes/x.qcow2", 409, body).unwrap();
        assert_eq!(status, "failed", "a 409 is not a done deletion");
        assert!(
            message.contains("in use by web1"),
            "daemon reason lost: {message}"
        );
    }

    #[test]
    fn a_non_json_error_body_is_still_recorded() {
        let (_, status, message) =
            delete_activity("/domains/gone", 500, "upstream exploded").unwrap();
        assert_eq!(status, "failed");
        assert!(message.contains("upstream exploded"), "got: {message}");
    }

    /// Storage outcomes must name what was spared and why, since that is the
    /// question this log exists to answer.
    #[test]
    fn the_storage_summary_names_kept_disks_and_reasons() {
        let body = r#"{
            "removed": ["default/a.qcow2"],
            "kept": [
                {"disk": "ISOs/x.iso", "reason": "iso"},
                {"disk": "default/shared.qcow2", "reason": "in_use"}
            ],
            "failed": []
        }"#;
        let got = summarize_cleanup(body);
        assert!(got.contains("removed 1"), "got: {got}");
        assert!(got.contains("kept 2"), "got: {got}");
        assert!(got.contains("iso"), "reasons missing: {got}");
        assert!(got.contains("in_use"), "reasons missing: {got}");
    }

    #[test]
    fn the_storage_summary_reports_failures_and_tolerates_junk() {
        let body = r#"{"removed":[],"kept":[],"failed":[{"disk":"x","error":"boom"}]}"#;
        let got = summarize_cleanup(body);
        assert!(got.contains("could not be removed"), "got: {got}");
        // A response that is not JSON must not panic or invent detail.
        assert_eq!(summarize_cleanup("not json"), "");
        assert_eq!(summarize_cleanup("{}"), "removed 0 disk(s)");
    }
}
