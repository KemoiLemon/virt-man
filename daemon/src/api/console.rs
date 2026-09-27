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

/// How long a relay port waits for its one connection.
const RELAY_TTL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct VvQuery {
    /// `spice` or `vnc`; defaults to SPICE when the guest has it (virt-viewer
    /// does more with it: audio, clipboard, USB).
    kind: Option<String>,
    /// The address the viewer will connect from; only it is let through the
    /// relay. The web server sets it from the browser's socket address.
    /// Absent, only loopback is accepted.
    client: Option<String>,
    /// Host name to write into the file: the name the user already reaches
    /// this server by. Absent, the request's own `Host` is used.
    host: Option<String>,
}

/// A virt-viewer connection file.
///
/// Guest displays listen on 127.0.0.1, so a file naming that address works
/// only on the hypervisor. Instead the file names a **one-shot relay**: a port
/// opened for [`RELAY_TTL`] that accepts exactly one connection, only from the
/// address that asked for the file, and splices it to the guest's display.
/// Nothing stays exposed once the viewer connects or the minute is up, and the
/// guest's own listener never leaves loopback.
async fn vv_file(
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<VvQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let n = name.clone();
    let all = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &n)?;
        if !d.is_active().unwrap_or(false) {
            return Err("the guest is not running".to_string().into());
        }
        let xml = d.get_xml_desc(0)?;
        Ok(parse_all_graphics(&xml))
    })
    .await
    .map_err(ApiError::bad_request)?;
    let g = pick_graphics(&all, q.kind.as_deref())
        .ok_or_else(|| ApiError::not_found("the guest has no such graphics device"))?;

    let client: std::net::IpAddr = match q.client.as_deref() {
        Some(c) => c
            .parse()
            .map_err(|_| ApiError::bad_request("client is not an IP address"))?,
        None => std::net::Ipv6Addr::LOCALHOST.into(),
    };
    let listener = tokio::net::TcpListener::bind("[::]:0")
        .await
        .map_err(ApiError::internal)?;
    let port = listener.local_addr().map_err(ApiError::internal)?.port();
    tokio::spawn(relay_once(
        listener,
        client,
        (g.host.clone(), g.port),
        RELAY_TTL,
    ));

    let host = q
        .host
        .or_else(|| host_of(&headers))
        .unwrap_or_else(|| "localhost".into());
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-virt-viewer"),
            (
                header::CONTENT_DISPOSITION,
                // A fixed name keeps the header free of untrusted input.
                "attachment; filename=\"console.vv\"",
            ),
        ],
        vv_body(&g.kind, &host, port, &name),
    )
        .into_response())
}

/// The display to hand out: the one asked for, else SPICE, else the first.
fn pick_graphics(all: &[Graphics], kind: Option<&str>) -> Option<Graphics> {
    match kind {
        Some(k) => all.iter().find(|g| g.kind == k),
        None => all
            .iter()
            .find(|g| g.kind == "spice")
            .or_else(|| all.first()),
    }
    .cloned()
}

/// Host part of a `Host` header, without its port (IPv6 brackets kept).
fn host_of(headers: &axum::http::HeaderMap) -> Option<String> {
    let h = headers.get(header::HOST)?.to_str().ok()?;
    let bare = match h.rfind(']') {
        Some(end) => &h[..=end],
        None => h.rsplit_once(':').map_or(h, |(a, _)| a),
    };
    (!bare.is_empty()).then(|| bare.to_string())
}

fn vv_body(kind: &str, host: &str, port: u16, title: &str) -> String {
    // A value ends at a newline in an INI file, so a guest name containing one
    // could otherwise add keys of its own (a `proxy=`, say).
    let clean = |s: &str| s.replace(['\r', '\n'], " ");
    format!(
        "[virt-viewer]\ntype={}\nhost={}\nport={port}\ntitle={}\ndelete-this-file=1\nfullscreen=0\n",
        clean(kind),
        clean(host),
        clean(title),
    )
}

/// Same address, allowing for IPv4 arriving on a dual-stack socket as
/// `::ffff:a.b.c.d`.
fn same_ip(a: std::net::IpAddr, b: std::net::IpAddr) -> bool {
    let canon = |ip: std::net::IpAddr| match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
        v4 => v4,
    };
    canon(a) == canon(b)
}

/// Accept one connection from `client` within `ttl` and splice it to
/// `target`. A connection from anyone else is dropped and does not use up the
/// relay; the listener closes at the first accepted connection or the
/// timeout, whichever comes first.
async fn relay_once(
    listener: tokio::net::TcpListener,
    client: std::net::IpAddr,
    target: (String, u16),
    ttl: Duration,
) {
    let accepted = tokio::time::timeout(ttl, async {
        loop {
            let Ok((sock, peer)) = listener.accept().await else {
                return None;
            };
            if same_ip(peer.ip(), client) {
                return Some(sock);
            }
            tracing::warn!("vv relay: refused {peer} (expected {client})");
        }
    })
    .await;
    drop(listener);
    let Ok(Some(mut inbound)) = accepted else {
        return;
    };
    let Ok(mut outbound) = tokio::net::TcpStream::connect((target.0.as_str(), target.1)).await
    else {
        return;
    };
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
}

#[cfg(test)]
mod tests {
    use super::{host_of, pick_graphics, relay_once, same_ip, vv_body};
    use crate::libvirt::Graphics;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn g(kind: &str, port: u16) -> Graphics {
        Graphics {
            kind: kind.into(),
            host: "127.0.0.1".into(),
            port,
        }
    }

    #[test]
    fn spice_is_preferred_unless_a_kind_is_asked_for() {
        let both = [g("vnc", 5900), g("spice", 5901)];
        assert_eq!(pick_graphics(&both, None).unwrap().kind, "spice");
        assert_eq!(pick_graphics(&both, Some("vnc")).unwrap().port, 5900);
        assert_eq!(pick_graphics(&[g("vnc", 5900)], None).unwrap().kind, "vnc");
        assert!(pick_graphics(&[g("vnc", 5900)], Some("spice")).is_none());
    }

    #[test]
    fn the_file_names_the_relay_not_the_guests_loopback_listener() {
        let body = vv_body("spice", "hv.example", 41234, "web");
        assert!(
            body.contains("host=hv.example\n") && body.contains("port=41234\n"),
            "{body}"
        );
        assert!(!body.contains("127.0.0.1"));
    }

    #[test]
    fn a_newline_in_a_value_cannot_add_keys() {
        let body = vv_body("spice", "h", 1, "x\nproxy=http://evil");
        assert!(!body.lines().any(|l| l.starts_with("proxy=")), "{body}");
    }

    #[test]
    fn the_host_header_loses_its_port_but_keeps_ipv6_brackets() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("host", "hv.example:8443".parse().unwrap());
        assert_eq!(host_of(&h).as_deref(), Some("hv.example"));
        h.insert("host", "[fd00::1]:8443".parse().unwrap());
        assert_eq!(host_of(&h).as_deref(), Some("[fd00::1]"));
    }

    #[test]
    fn ipv4_mapped_addresses_match_their_ipv4_form() {
        let v4: std::net::IpAddr = "192.168.1.5".parse().unwrap();
        let mapped: std::net::IpAddr = "::ffff:192.168.1.5".parse().unwrap();
        assert!(same_ip(v4, mapped) && same_ip(mapped, v4));
        assert!(!same_ip(v4, "192.168.1.6".parse().unwrap()));
    }

    #[tokio::test]
    async fn the_relay_serves_one_connection_from_the_right_address_only() {
        // Stand-in for the guest's display: echoes five bytes.
        let display = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dport = display.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = display.accept().await {
                let mut b = [0u8; 5];
                if s.read_exact(&mut b).await.is_ok() {
                    let _ = s.write_all(&b).await;
                }
            }
        });
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rport = relay.local_addr().unwrap().port();
        let task = tokio::spawn(relay_once(
            relay,
            "127.0.0.1".parse().unwrap(),
            ("127.0.0.1".into(), dport),
            std::time::Duration::from_secs(5),
        ));
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", rport))
            .await
            .unwrap();
        c.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        c.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello", "spliced to the display");
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", rport))
                .await
                .is_err(),
            "one-shot: the port closes once a viewer is attached"
        );
        drop(c);
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_stranger_is_refused_and_an_unused_relay_expires() {
        // A live display that greets whoever reaches it, so a stranger let
        // through would read the greeting instead of a closed socket.
        let display = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dport = display.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = display.accept().await {
                let _ = s.write_all(b"HI").await;
            }
        });
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rport = relay.local_addr().unwrap().port();
        let task = tokio::spawn(relay_once(
            relay,
            "10.9.9.9".parse().unwrap(),
            ("127.0.0.1".into(), dport),
            std::time::Duration::from_millis(400),
        ));
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", rport))
            .await
            .unwrap();
        let mut b = [0u8; 2];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), c.read(&mut b))
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(
            n, 0,
            "a connection from the wrong address never reaches the display"
        );
        task.await.unwrap();
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", rport))
                .await
                .is_err(),
            "the port is closed after the TTL"
        );
    }
}
