use virt::connect::Connect;

pub const URI: &str = "qemu:///system";

/// Error type for closures run inside [`with_conn`]. Accepts libvirt errors,
/// plain strings, and I/O errors via `?`.
#[derive(Debug)]
pub enum LvError {
    Virt(virt::error::Error),
    Msg(String),
}

impl std::fmt::Display for LvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LvError::Virt(e) => write!(f, "{e}"),
            LvError::Msg(m) => write!(f, "{m}"),
        }
    }
}

impl From<virt::error::Error> for LvError {
    fn from(e: virt::error::Error) -> Self {
        LvError::Virt(e)
    }
}

impl From<String> for LvError {
    fn from(s: String) -> Self {
        LvError::Msg(s)
    }
}

impl From<std::io::Error> for LvError {
    fn from(e: std::io::Error) -> Self {
        LvError::Msg(e.to_string())
    }
}

/// Run a closure with a fresh libvirt connection on a blocking thread.
///
/// ponytail: open a connection per operation instead of keeping a persistent
/// one + actor thread; libvirt wants each connection confined to one thread,
/// and the local socket handshake is cheap. Revisit if request volume grows.
pub async fn with_conn<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Connect) -> Result<T, LvError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut conn = Connect::open(Some(URI)).map_err(|e| format!("libvirt connect: {e}"))?;
        let out = f(&conn).map_err(|e| e.to_string());
        // `virt::Connect` has no Drop impl, so an unclosed connection leaks
        // its libvirt event fds. A polling client exhausts the process fd
        // limit after a few hundred requests, and every later call then fails
        // with "Too many open files". Close on both the ok and error paths.
        if let Err(e) = conn.close() {
            tracing::warn!("libvirt connection close failed: {e}");
        }
        out
    })
    .await
    .map_err(|e| format!("libvirt task panicked: {e}"))?
}

pub fn domain_state_name(state: virt::sys::virDomainState) -> &'static str {
    match state as i32 {
        0 => "nostate",
        1 => "running",
        2 => "blocked",
        3 => "paused",
        4 => "shutdown",
        5 => "shutoff",
        6 => "crashed",
        7 => "pmsuspended",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Graphics {
    pub kind: String,
    pub host: String,
    pub port: u16,
}

/// Every `<graphics>` element in the XML, in document order.
///
/// A domain may legitimately define one VNC and one SPICE server at the same
/// time, so callers pick the one they can actually use rather than assuming
/// there is only ever one.
pub fn parse_all_graphics(xml: &str) -> Vec<Graphics> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<graphics") {
        let Some(rel_end) = rest[start..].find('>') else { break };
        let end = start + rel_end;
        let tag = &rest[start..=end];

        if let (Some(kind), Some(port)) = (attr(tag, "type"), attr(tag, "port"))
            && let Ok(port) = port.parse::<u16>()
        {
            // A nested <listen> may carry the address instead of the
            // attribute, so look inside the element too.
            let body_end = rest[end..]
                .find("</graphics>")
                .map(|i| end + i)
                .unwrap_or(rest.len());
            let host = attr(tag, "listen")
                .or_else(|| nested_listen_address(&rest[start..body_end]))
                .unwrap_or("127.0.0.1")
                .to_string();
            out.push(Graphics { kind: kind.to_string(), host, port });
        }
        rest = &rest[end + 1..];
    }
    out
}

/// The graphics device a native viewer should use.
///
/// VNC wins when a domain offers both, because that is the only protocol the
/// built-in viewer speaks; SPICE-only domains still report themselves so the
/// caller can offer virt-viewer instead.
pub fn parse_graphics(xml: &str) -> Option<Graphics> {
    let all = parse_all_graphics(xml);
    all.iter()
        .find(|g| g.kind == "vnc")
        .or_else(|| all.first())
        .cloned()
}

/// Read an XML attribute's value out of a tag.
///
/// Only a real attribute position counts: `<listen type='address'
/// address='10.0.0.5'/>` contains "address" inside the *value* of `type` first,
/// and matching that returned the wrong span — so a guest listening on a real
/// IP silently fell back to loopback and was unreachable.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(rel) = tag[from..].find(name) {
        let i = from + rel;
        from = i + name.len();
        // An attribute name is preceded by whitespace (or starts the tag) and
        // followed by '='; anything else is a substring of a value or of a
        // longer name such as "listen" inside "listenAddress".
        let boundary_before = i == 0
            || tag[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        if !boundary_before {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next() else {
            continue;
        };
        let Some(inner) = rest.strip_prefix(quote) else {
            continue;
        };
        let Some(j) = inner.find(quote) else {
            continue;
        };
        return Some(&inner[..j]);
    }
    None
}

fn nested_listen_address(block: &str) -> Option<&str> {
    let i = block.find("<listen")?;
    attr(&block[i..], "address")
}

#[cfg(test)]
mod graphics_tests {
    use super::*;

    /// The exact shape libvirt produces for a guest offering both servers.
    const DUAL: &str = r#"
    <domain><devices>
      <graphics type='vnc' port='5900' autoport='yes' listen='127.0.0.1'>
        <listen type='address' address='127.0.0.1'/>
      </graphics>
      <graphics type='spice' port='5901' autoport='yes' listen='127.0.0.1'>
        <listen type='address' address='127.0.0.1'/>
        <image compression='off'/>
      </graphics>
    </devices></domain>"#;

    #[test]
    fn both_graphics_devices_are_found() {
        let all = parse_all_graphics(DUAL);
        assert_eq!(all.len(), 2, "a dual-graphics guest must report both");
        assert_eq!(all[0].kind, "vnc");
        assert_eq!(all[0].port, 5900);
        assert_eq!(all[1].kind, "spice");
        assert_eq!(all[1].port, 5901);
    }

    /// Regression: this took the first <graphics> element, so adding a VNC
    /// device in front of SPICE silently changed which server every caller got.
    #[test]
    fn the_native_viewer_prefers_vnc() {
        let g = parse_graphics(DUAL).expect("a device must be picked");
        assert_eq!(g.kind, "vnc", "only VNC can be rendered natively");
        assert_eq!(g.port, 5900);
    }

    #[test]
    fn a_spice_only_guest_still_reports_itself() {
        // So the caller can offer virt-viewer rather than claim no console.
        let xml = "<graphics type='spice' port='5900' listen='127.0.0.1'/>";
        let g = parse_graphics(xml).expect("spice must still be reported");
        assert_eq!(g.kind, "spice");
    }

    #[test]
    fn a_nested_listen_address_is_used_per_device() {
        // The address lives in a child element, not the attribute. Getting the
        // element boundary wrong would read the *next* device's address.
        let xml = r#"
          <graphics type='vnc' port='5900'>
            <listen type='address' address='10.0.0.5'/>
          </graphics>
          <graphics type='spice' port='5901'>
            <listen type='address' address='10.0.0.9'/>
          </graphics>"#;
        let all = parse_all_graphics(xml);
        assert_eq!(all[0].host, "10.0.0.5", "vnc keeps its own address");
        assert_eq!(all[1].host, "10.0.0.9", "spice keeps its own address");
    }

    #[test]
    fn a_domain_without_graphics_reports_none() {
        assert!(parse_graphics("<domain><devices/></domain>").is_none());
        assert!(parse_all_graphics("<domain/>").is_empty());
    }

    #[test]
    fn an_autoport_device_awaiting_a_port_is_skipped() {
        // An inactive domain has port='-1' until libvirt assigns one; that is
        // not a connectable endpoint, and must not parse as port 0.
        let all = parse_all_graphics("<graphics type='vnc' port='-1' autoport='yes'/>");
        assert!(all.is_empty(), "an unassigned port is not connectable");
    }
}
