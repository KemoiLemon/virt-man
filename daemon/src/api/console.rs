//! Console API: a native RFB viewer plus a virt-viewer handoff for SPICE.
//!
//! VNC guests are driven by `crate::rfb`, which keeps the framebuffer here on
//! the daemon and hands out only the rectangle that changed. `frame` is a long
//! poll: it parks inside the daemon until a newer frame exists, so pixels leave
//! on a request that was already waiting rather than on a poll interval.
//!
//! SPICE cannot be served this way — the protocol is not RFB and the browser
//! has no usable client — so those guests get a `.vv` connection file to open
//! in virt-viewer instead.

use axum::Router;
use axum::extract::{Json as ExtractJson, Path};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use virt::domain::Domain;

use super::ApiError;
use crate::libvirt::{Graphics, parse_all_graphics, parse_graphics, with_conn};

/// How long `frame` waits for new pixels before returning empty. Long enough
/// that an idle guest costs almost nothing, short enough that the caller can
/// notice a dropped session and re-arm.
const FRAME_WAIT: Duration = Duration::from_secs(20);

pub fn routes() -> Router {
    Router::new()
        .route("/domains/{name}/console/connect", post(connect))
        .route("/domains/{name}/console/frame", get(frame))
        .route("/domains/{name}/console/pointer", post(pointer))
        .route("/domains/{name}/console/key", post(key))
        .route("/domains/{name}/console/disconnect", post(disconnect))
        .route("/domains/{name}/console/status", get(status))
        .route("/domains/{name}/console/vv", get(vv_file))
}

/// Look up where a domain's graphics server is listening.
async fn graphics_of(name: &str) -> Result<Graphics, ApiError> {
    let n = name.to_string();
    with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &n)?;
        let xml = d.get_xml_desc(0)?;
        Ok(parse_graphics(&xml))
    })
    .await
    .map_err(ApiError::bad_request)?
    .ok_or_else(|| ApiError::not_found("domain has no graphics device"))
}

/// Attach to the guest's VNC server, reusing a live session when there is one.
///
/// Returns the guest resolution so the viewer can size its canvas before the
/// first frame lands.
async fn connect(Path(name): Path<String>) -> Result<Response, ApiError> {
    let g = graphics_of(&name).await?;
    if g.kind != "vnc" {
        return Err(ApiError::bad_request(format!(
            "{} consoles are not supported natively; open the virt-viewer file instead",
            g.kind
        )));
    }

    // An existing session keeps its framebuffer, so revisiting the tab is
    // instant and the guest never sees a reconnect.
    if let Some(s) = crate::rfb::get(&name).filter(|s| s.is_running()) {
        let f = s.frame.lock().unwrap();
        return Ok(axum::Json(json!({"width": f.w, "height": f.h})).into_response());
    }

    let host = g.host.clone();
    let port = g.port;
    let id = name.clone();
    // rfb::connect blocks on the RFB handshake; keep it off the async runtime.
    let sess = tokio::task::spawn_blocking(move || crate::rfb::connect(&id, &host, port))
        .await
        .map_err(|e| ApiError::internal(format!("console task: {e}")))?
        .map_err(ApiError::bad_request)?;

    let (w, h) = {
        let f = sess.frame.lock().unwrap();
        (f.w, f.h)
    };
    Ok(axum::Json(json!({"width": w, "height": h})).into_response())
}

#[derive(Deserialize)]
struct Since {
    /// Last sequence the caller has painted; 0 asks for a full frame.
    #[serde(default)]
    since: u64,
}

/// Long-poll for the next changed rectangle.
///
/// Returns `{"seq": 0}` when nothing changed before the timeout, which the
/// viewer treats as "ask again".
async fn frame(
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<Since>,
) -> Result<Response, ApiError> {
    let sess = crate::rfb::get(&name).ok_or_else(|| ApiError::not_found("no console session"))?;
    if let Some(e) = sess.take_error() {
        return Err(ApiError::bad_request(e));
    }

    // next_update parks on a condvar, so it must not run on a runtime thread.
    let update = tokio::task::spawn_blocking(move || sess.next_update(q.since, FRAME_WAIT))
        .await
        .map_err(|e| ApiError::internal(format!("frame task: {e}")))?;

    let Some(u) = update else {
        return Ok(axum::Json(json!({"seq": 0})).into_response());
    };

    Ok(axum::Json(json!({
        "seq": u.seq,
        "width": u.w,
        "height": u.h,
        "x": u.rect.x,
        "y": u.rect.y,
        "w": u.rect.w,
        "h": u.rect.h,
        "pixels": base64::engine::general_purpose::STANDARD.encode(&u.pixels),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct Pointer {
    x: u16,
    y: u16,
    buttons: u8,
}

async fn pointer(
    Path(name): Path<String>,
    ExtractJson(p): ExtractJson<Pointer>,
) -> Result<Response, ApiError> {
    let sess = crate::rfb::get(&name).ok_or_else(|| ApiError::not_found("no console session"))?;
    sess.pointer(p.x, p.y, p.buttons)
        .map_err(ApiError::bad_request)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Key {
    keysym: u32,
    down: bool,
}

async fn key(
    Path(name): Path<String>,
    ExtractJson(k): ExtractJson<Key>,
) -> Result<Response, ApiError> {
    let sess = crate::rfb::get(&name).ok_or_else(|| ApiError::not_found("no console session"))?;
    sess.key(k.keysym, k.down).map_err(ApiError::bad_request)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn disconnect(Path(name): Path<String>) -> Response {
    crate::rfb::disconnect(&name);
    StatusCode::NO_CONTENT.into_response()
}

/// Whether a session is still alive, and why it died if not. The viewer polls
/// this as a safety net: frames come from `frame`, never from here.
async fn status(Path(name): Path<String>) -> Response {
    let live = crate::rfb::active_ids().contains(&name);
    let error = crate::rfb::get(&name).and_then(|s| s.take_error());
    axum::Json(json!({"connected": live, "error": error})).into_response()
}

/// A virt-viewer connection file for guests we cannot render natively.
///
/// SPICE is preferred here, the mirror of `parse_graphics`: virt-viewer handles
/// it better than VNC (audio, clipboard, USB), and a guest offering both is
/// exactly the case where the built-in viewer already took the VNC device.
///
/// The host comes from the domain's listen address, usually 127.0.0.1, which is
/// only reachable on the hypervisor itself.
async fn vv_file(Path(name): Path<String>) -> Result<Response, ApiError> {
    let n = name.clone();
    let all = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &n)?;
        let xml = d.get_xml_desc(0)?;
        Ok(parse_all_graphics(&xml))
    })
    .await
    .map_err(ApiError::bad_request)?;
    let g = all
        .iter()
        .find(|g| g.kind == "spice")
        .or_else(|| all.first())
        .cloned()
        .ok_or_else(|| ApiError::not_found("domain has no graphics device"))?;
    let body = format!(
        "[virt-viewer]\ntype={}\nhost={}\nport={}\ntitle={}\ndelete-this-file=1\nfullscreen=0\n",
        g.kind, g.host, g.port, name
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-virt-viewer"),
            (
                header::CONTENT_DISPOSITION,
                // A fixed name keeps the header free of untrusted input.
                "attachment; filename=\"console.vv\"",
            ),
        ],
        body,
    )
        .into_response())
}
