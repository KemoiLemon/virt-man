use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;

use super::ApiError;
use crate::auth;
use crate::daemon;
use crate::state::AppState;
use crate::store::User;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/proxy/{conn_id}/console/domains/{name}",
            axum::routing::get(console),
        )
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

/// All non-console daemon calls proxy through here. The daemon token never
/// reaches the browser: the backend attaches it server-side.
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

/// Browser WebSocket -> daemon WebSocket (VNC/SPICE console), TLS-pinned.
#[allow(clippy::result_large_err)]
async fn console(
    State(state): State<AppState>,
    Path((conn_id, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    ws: WebSocketUpgrade,
) -> Result<Response, Response> {
    if let Err(e) = super::enforce(&state, &user, "vms", false) {
        return Err(e.into_response());
    }
    let conn = {
        let store = state.read();
        let Some(c) = store.connection(&conn_id) else {
            return Err(StatusCode::NOT_FOUND.into_response());
        };
        c.clone()
    };

    let addr = daemon::parse_url(&conn.url)
        .map_err(|e| (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response())?;

    let scheme = if addr.tls { "wss" } else { "ws" };
    let target = format!(
        "{scheme}://{}:{}/domains/{name}/console?token={}",
        addr.host, addr.port, conn.token
    );
    let connector = daemon::ws_connector(&conn)
        .map_err(|e| (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response())?;

    let _ = q;
    // spice-html5 opens its socket as `new WebSocket(uri, 'binary')`. A server
    // that does not echo the negotiated subprotocol makes the client fail the
    // connection per the WebSocket spec, which showed up as a SPICE console
    // that connected at the TCP level and then silently never rendered.
    Ok(ws
        .protocols(["binary"])
        .on_upgrade(move |browser| async move {
            if let Err(e) = console_pipe(browser, target, connector).await {
                tracing::warn!("console proxy for {conn_id}/{name} ended: {e}");
            }
        }))
}

async fn console_pipe(
    browser: WebSocket,
    target: String,
    connector: tokio_tungstenite::Connector,
) -> Result<(), String> {
    // Ask the daemon for the same `binary` subprotocol the browser negotiated,
    // so the whole chain agrees and spice-html5's client does not fail the
    // connection.
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        target.as_str(),
    )
    .map_err(|e| format!("build upstream ws request: {e}"))?;
    let mut request = request;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        axum::http::HeaderValue::from_static("binary"),
    );

    let (daemon_ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .map_err(|e| format!("daemon websocket: {e}"))?;

    let (mut browser_tx, mut browser_rx) = browser.split();
    let (mut daemon_tx, mut daemon_rx) = daemon_ws.split();

    loop {
        tokio::select! {
            msg = daemon_rx.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b))) => {
                        browser_tx.send(Message::Binary(b)).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                        browser_tx.send(Message::Text(t.as_str().to_string().into())).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))
                    | Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_)))
                    | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
            msg = browser_rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(d))) => {
                        daemon_tx.send(tokio_tungstenite::tungstenite::Message::Binary(d.to_vec().into())).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(Message::Text(t))) => {
                        daemon_tx.send(tokio_tungstenite::tungstenite::Message::Text(t.as_str().to_string().into())).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(Message::Close(_))) => {
                        let _ = daemon_tx.send(tokio_tungstenite::tungstenite::Message::Close(None)).await;
                        break;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                    Some(Err(e)) => return Err(e.to_string()),
                    None => break,
                }
            }
        }
    }
    Ok(())
}
