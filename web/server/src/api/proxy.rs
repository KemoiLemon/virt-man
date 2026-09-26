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

    let mut out = Response::builder().status(status);
    for (name, value) in resp_headers.iter() {
        if name.as_str() != "transfer-encoding" && name.as_str() != "connection" {
            out = out.header(name, value);
        }
    }
    let _ = user;
    out.body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("build response: {e}")))
}
