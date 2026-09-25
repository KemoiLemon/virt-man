use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, RawQuery};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use virt::domain::Domain;

use crate::auth::{Auth, check_query_token};
use crate::libvirt::{Graphics, parse_graphics, with_conn};

pub fn routes() -> Router {
    Router::new().route("/domains/{name}/console", get(ws_handler))
}

#[allow(clippy::result_large_err)]
async fn ws_handler(
    Extension(auth): Extension<Auth>,
    Path(name): Path<String>,
    RawQuery(raw_query): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response, Response> {
    if !check_query_token(raw_query.as_deref().unwrap_or(""), &auth) {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }

    let name_for_upgrade = name.clone();
    let graphics = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let xml = d.get_xml_desc(0)?;
        Ok(parse_graphics(&xml))
    })
    .await
    .map_err(|e| (StatusCode::NOT_FOUND, axum::Json(json!({"error": e}))).into_response())?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "domain has no graphics device"})),
        )
            .into_response()
    })?;

    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = proxy(socket, graphics).await {
            tracing::warn!("console proxy for {name_for_upgrade} ended: {e}");
        }
    }))
}

/// Raw TCP <-> WebSocket byte pipe. Works for VNC (noVNC).
///
/// ponytail: SPICE-over-websocket needs spice-html5's "binary" subprotocol
/// framing on top of this pipe; add when SPICE consoles are required.
async fn proxy(socket: WebSocket, g: Graphics) -> Result<(), String> {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut tcp_rx, mut tcp_tx) = TcpStream::connect(format!("{}:{}", g.host, g.port))
        .await
        .map_err(|e| e.to_string())?
        .into_split();

    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            n = tcp_rx.read(&mut buf) => {
                let n = n.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                ws_tx
                    .send(Message::Binary(buf[..n].to_vec().into()))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(d))) => {
                        tcp_tx.write_all(&d).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(Message::Text(t))) => {
                        tcp_tx.write_all(t.as_bytes()).await.map_err(|e| e.to_string())?;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
        }
    }
    Ok(())
}
