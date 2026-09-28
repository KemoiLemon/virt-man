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
use crate::guard;
use crate::perm::Kind;
use crate::routes::{self, Special};
use crate::state::AppState;
use crate::store::User;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Request header: the SPA would rather have the last known answer now than
/// the current one a round trip later. It always follows up with a plain read.
const PREFER_CACHE: &str = "x-virt-man-cache";
/// Response header on an answer served from the cache: its age in ms.
const CACHED_AGE: &str = "x-virt-man-cached";
/// Only small JSON reads are worth keeping; console frames and the like are not.
const MAX_CACHED_BODY: usize = 256 * 1024;
/// ponytail: cleared wholesale when full; an LRU if many hosts ever matter.
const MAX_CACHED_ENTRIES: usize = 1024;

/// The last successful JSON read per (connection, path+query).
///
/// A remote daemon costs a network round trip per read, so opening a page used
/// to show nothing until it answered. The SPA asks for this copy to paint at
/// once and then re-reads for real, so a stale entry is on screen for one round
/// trip at most. The raw daemon answer is cached, and served only after the
/// privilege check passes and through the same per-user filtering as a fresh
/// answer, so an entry leaks nothing the caller could not have read directly.
#[derive(Default)]
struct ReadCache {
    entries: HashMap<(String, String), (Instant, String, axum::body::Bytes)>,
}

impl ReadCache {
    fn get(&self, conn: &str, path: &str) -> Option<(u128, String, axum::body::Bytes)> {
        let (at, ct, body) = self.entries.get(&(conn.to_string(), path.to_string()))?;
        Some((at.elapsed().as_millis(), ct.clone(), body.clone()))
    }

    fn put(&mut self, conn: &str, path: &str, content_type: &str, body: axum::body::Bytes) {
        if !content_type.starts_with("application/json") || body.len() > MAX_CACHED_BODY {
            return;
        }
        if self.entries.len() >= MAX_CACHED_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(
            (conn.to_string(), path.to_string()),
            (Instant::now(), content_type.to_string(), body),
        );
    }

    /// Any write can change any read (deleting a VM changes the domain list,
    /// the pool's volumes, the host figures), so drop the whole connection
    /// rather than guess which paths it touched. Otherwise a fast first paint
    /// could briefly resurrect what the user just deleted.
    fn invalidate(&mut self, conn: &str) {
        self.entries.retain(|(c, _), _| c != conn);
    }
}

fn read_cache() -> &'static Mutex<ReadCache> {
    static CACHE: OnceLock<Mutex<ReadCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub fn routes(state: &AppState) -> Router<AppState> {
    // The console is plain HTTP now (the RFB client lives in the daemon and
    // frames are fetched), so it needs no route of its own.
    Router::new()
        .route("/proxy/{conn_id}/{*rest}", any(proxy))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

/// The daemon URL for a `.vv` request, with the viewer's address and the
/// daemon's host appended. Both are set here, never taken from the browser: a
/// client-chosen `client` would let anyone open the relay to any address.
///
fn vv_target(target: &str, viewer: std::net::IpAddr, daemon_host: &str) -> String {
    let viewer = match viewer {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(viewer, std::net::IpAddr::V4),
        v4 => v4,
    };
    // Strip anything the browser put in the query for these two keys.
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with("client=") && !kv.starts_with("host="))
        .collect();
    let mut q = kept.join("&");
    if !q.is_empty() {
        q.push('&');
    }
    let enc = |s: &str| {
        s.replace('%', "%25")
            .replace(':', "%3A")
            .replace('[', "%5B")
            .replace(']', "%5D")
    };
    format!(
        "{path}?{q}client={}&host={}",
        enc(&viewer.to_string()),
        enc(daemon_host)
    )
}

/// The host a viewer should dial for the relay.
///
/// The daemon's own URL is the right answer when it names a real host. When it
/// is loopback (web UI and daemon on one machine, the usual setup) it is useless
/// to a viewer elsewhere; the host the browser reached *us* by is the same
/// machine and is known to be reachable from the viewer, so use that.
fn vv_host(headers: &HeaderMap, daemon_host: &str) -> String {
    let loopback = matches!(daemon_host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || daemon_host.starts_with("127.");
    if !loopback {
        return daemon_host.to_string();
    }
    let browser = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| match h.rfind(']') {
            Some(end) => &h[..=end],
            None => h.rsplit_once(':').map_or(h, |(a, _)| a),
        })
        .unwrap_or("");
    if browser.is_empty() {
        daemon_host.to_string()
    } else {
        browser.to_string()
    }
}

/// Largest request body the proxy will buffer to check. Definitions are a
/// few KiB; anything larger than this is not one.
const MAX_CHECKED_BODY: usize = 4 * 1024 * 1024;

/// Filter a successful list answer to what `user` may see.
fn filter(
    state: &AppState,
    user: &User,
    conn: &str,
    special: &Special,
    body: &[u8],
) -> Option<Vec<u8>> {
    match special {
        Special::ListVms => guard::filter_list(state, user, conn, Kind::Vm, body),
        Special::ListNetworks => guard::filter_list(state, user, conn, Kind::Network, body),
        Special::ListPools => guard::filter_pools(state, user, conn, body),
        Special::ListBridges => guard::filter_bridges(state, user, conn, body),
        Special::ListVolumes { pool } => guard::annotate_volumes(state, user, conn, pool, body),
        _ => None,
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
// axum extractors are the arguments; bundling them into a struct would only
// move the list somewhere else.
#[allow(clippy::too_many_arguments)]
async fn proxy(
    State(state): State<AppState>,
    Path((conn_id, _rest)): Path<(String, String)>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let write = method != axum::http::Method::GET && method != axum::http::Method::HEAD;
    // Captured before `method` is moved into the request below.
    let destructive = method == axum::http::Method::DELETE;
    let rest_path = uri
        .path()
        .strip_prefix(&format!("/proxy/{conn_id}"))
        .unwrap_or("/");
    // Every daemon route needs one privilege at one path; a route the table
    // does not know is refused rather than guessed at.
    let need = routes::classify(&conn_id, method.as_str(), rest_path).ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            format!(
                "{method} {rest_path} is not an operation this server allows through the web UI"
            ),
        )
    })?;
    super::enforce(&state, &user, &need.path, need.privilege)?;

    let conn = {
        let store = state.read();
        let Some(c) = store.connection(&conn_id) else {
            return Err(ApiError::not_found("no such connection"));
        };
        c.clone()
    };

    let addr = daemon::parse_url(&conn.url)?;
    let base = addr.base.clone();
    // uri comes in as /proxy/{id}/rest/of/path — strip the prefix.
    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let rest = path_and_query
        .strip_prefix(&format!("/proxy/{conn_id}"))
        .unwrap_or(path_and_query);
    let target = format!("{}{}", base, rest);
    // A virt-viewer file names a relay the daemon opens for one viewer: tell
    // it who that viewer is (the browser, not this server) and the host name
    // to write, so the file works from the machine that downloaded it.
    let target = if uri.path().ends_with("/console/vv") {
        vv_target(&target, peer.ip(), &vv_host(&headers, &addr.host))
    } else {
        target
    };

    if !write && headers.contains_key(PREFER_CACHE) {
        let hit = read_cache()
            .lock()
            .expect("read cache poisoned")
            .get(&conn_id, rest);
        if let Some((age_ms, content_type, bytes)) = hit {
            let bytes = filter(&state, &user, &conn_id, &need.special, &bytes)
                .map(axum::body::Bytes::from)
                .unwrap_or(bytes);
            return Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(CACHED_AGE, age_ms.to_string())
                .body(Body::from(bytes))
                .map_err(|e| ApiError::internal(format!("build response: {e}")));
        }
    }

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

    // A body the guard must read is buffered; anything else streams straight
    // through (ISO uploads are large, and only an admin can send one here).
    let checks_body = !matches!(
        need.special,
        Special::None
            | Special::AdminOnly
            | Special::DeleteVm { .. }
            | Special::DeleteNetwork { .. }
            | Special::DeleteVolume { .. }
    ) && write;
    let (req_body, checked, created) = if checks_body {
        let bytes = axum::body::to_bytes(body, MAX_CHECKED_BODY)
            .await
            .map_err(|_| ApiError::bad_request("request body too large"))?;
        let created = guard::before(&state, &user, &conn, &need.special, &bytes).await?;
        (reqwest::Body::from(bytes.clone()), bytes, created)
    } else {
        guard::before(&state, &user, &conn, &need.special, &[]).await?;
        let stream = body
            .into_data_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
        (
            reqwest::Body::wrap_stream(stream),
            axum::body::Bytes::new(),
            None,
        )
    };
    let resp = builder.body(req_body).send().await.map_err(|e| {
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

    {
        let mut cache = read_cache().lock().expect("read cache poisoned");
        if write {
            cache.invalidate(&conn_id);
        } else if status.is_success() {
            let content_type = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            cache.put(&conn_id, rest, content_type, bytes.clone());
        }
    }

    if status.is_success() && write {
        guard::after(&state, &conn_id, &need.special, created, &checked, &bytes);
    }
    let bytes = if status.is_success() {
        filter(&state, &user, &conn_id, &need.special, &bytes)
            .map(axum::body::Bytes::from)
            .unwrap_or(bytes)
    } else {
        bytes
    };

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
        // The length changes when a list is filtered; axum sets it again.
        if !matches!(
            name.as_str(),
            "transfer-encoding" | "connection" | "content-length"
        ) {
            out = out.header(name, value);
        }
    }
    out.body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("build response: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_loopback_daemon_hands_out_the_host_the_browser_used() {
        let mut h = super::HeaderMap::new();
        h.insert("host", "optibox.lan:8006".parse().unwrap());
        assert_eq!(super::vv_host(&h, "localhost"), "optibox.lan");
        assert_eq!(super::vv_host(&h, "127.0.0.1"), "optibox.lan");
        assert_eq!(
            super::vv_host(&h, "hv2.lan"),
            "hv2.lan",
            "a real daemon host wins"
        );
        h.insert("host", "[fd00::2]:8006".parse().unwrap());
        assert_eq!(super::vv_host(&h, "::1"), "[fd00::2]");
    }

    #[test]
    fn a_vv_request_carries_the_viewer_and_daemon_host_not_the_browsers_say_so() {
        let v4: std::net::IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        let out = super::vv_target(
            "https://hv:8443/domains/web/console/vv?kind=vnc&client=6.6.6.6&host=evil",
            v4,
            "hv.lan",
        );
        assert_eq!(
            out,
            "https://hv:8443/domains/web/console/vv?kind=vnc&client=192.168.1.20&host=hv.lan"
        );
        let v6: std::net::IpAddr = "fd00::5".parse().unwrap();
        let out = super::vv_target("https://hv/domains/x/console/vv", v6, "[fd00::1]");
        assert_eq!(
            out,
            "https://hv/domains/x/console/vv?client=fd00%3A%3A5&host=%5Bfd00%3A%3A1%5D"
        );
    }

    use super::{delete_activity, delete_label, summarize_cleanup, ReadCache, MAX_CACHED_BODY};

    #[test]
    fn a_cached_read_is_returned_for_the_same_connection_and_path_only() {
        let mut c = ReadCache::default();
        c.put("a", "/domains", "application/json", "[1]".into());
        let (_, ct, body) = c.get("a", "/domains").unwrap();
        assert_eq!((ct.as_str(), &body[..]), ("application/json", &b"[1]"[..]));
        assert!(c.get("b", "/domains").is_none(), "another host's answer");
        assert!(c.get("a", "/networks").is_none(), "another path's answer");
    }

    /// A write must drop that host's reads, or the fast first paint after a
    /// delete would show the deleted thing again.
    #[test]
    fn a_write_invalidates_only_that_connection() {
        let mut c = ReadCache::default();
        c.put("a", "/domains", "application/json", "[1]".into());
        c.put("a", "/pools/x/volumes", "application/json", "[]".into());
        c.put("b", "/domains", "application/json", "[2]".into());
        c.invalidate("a");
        assert!(c.get("a", "/domains").is_none());
        assert!(c.get("a", "/pools/x/volumes").is_none());
        assert!(
            c.get("b", "/domains").is_some(),
            "other hosts are untouched"
        );
    }

    /// Console frames, .vv files and large bodies are not worth holding.
    #[test]
    fn only_small_json_is_cached() {
        let mut c = ReadCache::default();
        c.put("a", "/domains/x/console/frame", "image/png", "png".into());
        c.put(
            "a",
            "/domains/x/console/vv",
            "application/x-virt-viewer",
            "vv".into(),
        );
        c.put(
            "a",
            "/big",
            "application/json",
            vec![b' '; MAX_CACHED_BODY + 1].into(),
        );
        assert!(c.get("a", "/domains/x/console/frame").is_none());
        assert!(c.get("a", "/domains/x/console/vv").is_none());
        assert!(c.get("a", "/big").is_none());
    }

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
