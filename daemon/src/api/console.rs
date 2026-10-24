//! Console API: a byte pipe to a guest's VNC server, plus a virt-viewer
//! handoff for SPICE.
//!
//! The browser runs noVNC; this side only carries its bytes. `stream` upgrades
//! the connection (HTTP 101, no WebSocket framing: the web server is the only
//! caller and puts the WebSocket on the browser leg) and splices it to the
//! display QEMU listens on, which stays on loopback.
//!
//! SPICE cannot be served this way — the protocol is not RFB and the browser
//! has no usable client — so those guests get a `.vv` connection file to open
//! in virt-viewer instead.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::time::Duration;

use super::ApiError;
use crate::qemu::{GraphicsInfo, Manager, State as VmState};

pub fn routes(manager: Manager) -> Router {
    Router::new()
        .route("/domains/{name}/console/stream", get(stream))
        .route("/domains/{name}/console/vv", get(vv_file))
        .with_state(manager)
}

fn map_err(e: String) -> ApiError {
    if e.contains("unknown QEMU") {
        ApiError::not_found(e)
    } else {
        ApiError::bad_request(e)
    }
}

/// Look up where a domain's graphics server is listening. The definition is the
/// QEMU manager's; VNC listens on loopback as the form writes it.
async fn graphics_of(manager: &Manager, name: &str) -> Result<GraphicsInfo, ApiError> {
    manager
        .graphics(name)
        .map_err(map_err)?
        .ok_or_else(|| ApiError::not_found("domain has no graphics device"))
}

/// Splice the caller to the guest's VNC server.
///
/// The display is dialled *before* the 101 is sent, so a guest that is off, or
/// has no VNC, is an ordinary error status the web server can pass on instead
/// of an upgrade that closes at once.
async fn stream(
    State(manager): State<Manager>,
    Path(name): Path<String>,
    mut req: Request,
) -> Result<Response, ApiError> {
    let g = graphics_of(&manager, &name).await?;
    if g.kind != "vnc" {
        return Err(ApiError::bad_request(format!(
            "{} consoles are not supported natively; open the virt-viewer file instead",
            g.kind
        )));
    }
    let addr = format!("{}:{}", g.host, g.port);
    let mut display = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| ApiError::bad_request(format!("connect {addr}: {e}")))?;
    display.set_nodelay(true).ok();

    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(up) = upgrade.await else { return };
        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(up), &mut display).await;
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "vnc")
        .body(axum::body::Body::empty())
        .expect("static response"))
}

/// How long a relay port stays open for its viewer.
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
/// only on the hypervisor. Instead the file names a **relay**: a port opened
/// for [`RELAY_TTL`] that splices connections from the address that asked for
/// the file to the guest's display, and from nobody else. The guest's own
/// listener never leaves loopback, and once the minute is up no new connection
/// is accepted.
async fn vv_file(
    State(manager): State<Manager>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<VvQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    if !matches!(
        manager.state(&name).map_err(map_err)?,
        VmState::Running | VmState::Paused
    ) {
        return Err(ApiError::bad_request("the guest is not running"));
    }
    let all: Vec<GraphicsInfo> = manager
        .graphics(&name)
        .map_err(map_err)?
        .into_iter()
        .collect();
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
        vv_body(g.kind, &host, port, &name),
    )
        .into_response())
}

/// The display to hand out: the one asked for, else SPICE, else the first.
fn pick_graphics(all: &[GraphicsInfo], kind: Option<&str>) -> Option<GraphicsInfo> {
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

/// Serve `target` on `listener` for `ttl`, splicing every connection from
/// `client` to it. A connection from anyone else is refused.
///
/// It must keep accepting rather than take one connection and close, because
/// the two protocols differ in how many sockets a viewer opens:
///
/// - VNC (RFB) is a single TCP connection, so one splice is the whole session.
/// - SPICE is a *set* of channels -- main, display, inputs, cursor, playback,
///   record, usbredir -- and "each channel uses a dedicated TCP socket" to the
///   same host and port (spice-space.org, "Spice for Newbies"). The client
///   opens the main channel, negotiates, then opens the rest.
///
/// The one-shot version therefore worked for VNC and could never work for
/// SPICE: the main channel was accepted, spliced, and the listener dropped, so
/// every further channel was refused and virt-viewer gave up before it could
/// display anything. Keeping the listener open for the TTL is what makes the
/// SPICE `.vv` usable.
async fn relay_once(
    listener: tokio::net::TcpListener,
    client: std::net::IpAddr,
    target: (String, u16),
    ttl: Duration,
) {
    let deadline = tokio::time::Instant::now() + ttl;
    let mut tasks = Vec::new();
    loop {
        let accepted = tokio::time::timeout_at(deadline, listener.accept()).await;
        let Ok(Ok((mut inbound, peer))) = accepted else {
            break; // TTL expired, or the listener failed: stop accepting.
        };
        if !same_ip(peer.ip(), client) {
            tracing::warn!("vv relay: refused {peer} (expected {client})");
            continue;
        }
        let target = target.clone();
        tasks.push(tokio::spawn(async move {
            let Ok(mut outbound) =
                tokio::net::TcpStream::connect((target.0.as_str(), target.1)).await
            else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        }));
    }
    // Wait for the channels already open. A viewer that is still connected
    // when the TTL runs out keeps working: only *new* channels stop being
    // accepted, which is what the window is for (the `.vv` is one-shot and
    // gone a minute after it is handed out).
    for t in tasks {
        let _ = t.await;
    }
}

#[cfg(test)]
mod tests {
    use super::{host_of, pick_graphics, relay_once, same_ip, vv_body};
    use crate::qemu::GraphicsInfo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn g(kind: &'static str, port: u16) -> GraphicsInfo {
        GraphicsInfo {
            kind,
            host: "127.0.0.1".into(),
            port,
        }
    }

    /// The console endpoint reads the display from the manager's definition:
    /// the VNC host and port the launch argv uses, and no graphics is an error.
    #[tokio::test]
    async fn graphics_come_from_the_stored_definition() {
        const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
        let dir = std::env::temp_dir().join(format!("console-gfx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.raw");
        std::fs::write(&disk, b"x").unwrap();
        let manager =
            crate::qemu::Manager::with_qemu(dir.join("data"), dir.join("state"), "/none").unwrap();
        let xml = format!(
            r#"<domain type="kvm"><name>{ID}</name><uuid>{ID}</uuid>
            <memory unit="KiB">65536</memory><vcpu>1</vcpu>
            <os><type arch="x86_64" machine="q35">hvm</type></os>
            <devices>
              <disk type="file"><driver name="qemu" type="raw"/><source file="{}"/><target dev="vda" bus="virtio"/></disk>
              <graphics type="vnc" port="5907" listen="127.0.0.1"/>
            </devices></domain>"#,
            disk.display()
        );
        manager.define(&xml).unwrap();
        let g = super::graphics_of(&manager, ID)
            .await
            .ok()
            .expect("the definition names a VNC display");
        assert_eq!(
            (g.kind, g.host.as_str(), g.port),
            ("vnc", "127.0.0.1", 5907)
        );
        assert!(
            super::graphics_of(&manager, "550e8400-e29b-41d4-a716-446655440001")
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
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
    async fn the_relay_serves_channels_from_the_right_address_only() {
        // Stand-in for the guest's display: echoes five bytes per connection.
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
        // Open SEVERAL connections, as SPICE does for its channels. The
        // one-shot relay accepted the first and then closed the port, so every
        // later channel was refused and virt-viewer could never bring up a
        // SPICE session at all.
        for channel in ["main", "display", "inputs"] {
            let mut c = tokio::net::TcpStream::connect(("127.0.0.1", rport))
                .await
                .unwrap_or_else(|e| panic!("{channel} channel refused: {e}"));
            c.write_all(b"hello").await.unwrap();
            let mut back = [0u8; 5];
            c.read_exact(&mut back).await.unwrap();
            assert_eq!(&back, b"hello", "{channel} channel spliced");
        }
        drop(task);
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
