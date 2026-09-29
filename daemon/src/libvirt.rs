use virt::connect::Connect;

pub const URI: &str = "qemu:///system";
pub const LXC_URI: &str = "lxc:///system";

/// Which libvirt driver a request is about. Containers are libvirt domains
/// too, so the `/domains` handlers serve them unchanged: `/containers/...`
/// runs them inside `DRIVER.scope(Driver::Lxc, ..)` (`api::containers`), and
/// `with_conn` opens the matching URI. A `/domains` request never sees a
/// container and a `/containers` request never sees a VM, which is what keeps
/// the web server's separate VM and CT permissions meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    Qemu,
    Lxc,
}

tokio::task_local! {
    pub static DRIVER: Driver;
}

/// The driver of the current request (qemu outside any scope).
pub fn driver() -> Driver {
    DRIVER.try_with(|d| *d).unwrap_or(Driver::Qemu)
}

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

/// Whether `s` is a UUID in libvirt's canonical form (36 chars, 8-4-4-4-12
/// hex). Only that exact shape is tried as a UUID, so a guest that happens to
/// be *named* like one is still found by its name.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A guest, network or pool by UUID or by name.
///
/// Names are chosen by users and can be reused -- delete a guest and make a
/// new one called the same, and a link, a grant or a history row keyed on the
/// name silently points at the new one. The UUID is libvirt's own identity
/// and never repeats, so every route takes it; a name still works, so the
/// CLI, old links and the web server's name-keyed records keep working.
/// UUID first, then name: a UUID-shaped *name* is found by name, since no
/// real UUID will match it.
macro_rules! by_id {
    ($fn:ident, $ty:ty) => {
        pub fn $fn(c: &Connect, id: &str) -> Result<$ty, virt::error::Error> {
            if is_uuid(id)
                && let Ok(x) = <$ty>::lookup_by_uuid_string(c, id)
            {
                return Ok(x);
            }
            <$ty>::lookup_by_name(c, id)
        }
    };
}
by_id!(domain, virt::domain::Domain);
by_id!(network, virt::network::Network);
by_id!(pool, virt::storage_pool::StoragePool);

/// The `<title>` of a definition, if it has one (and it is not empty).
/// Guests and networks keep their display name there; libvirt's own name is
/// the UUID, so a display name need not be unique.
pub fn title_of(xml: &str) -> Option<String> {
    let i = xml.find("<title>")?;
    let rest = &xml[i + 7..];
    let t = rest[..rest.find("</title>")?].trim();
    (!t.is_empty()).then(|| unescape(t))
}

/// Undo XML's five escapes, for text read out of a definition.
pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
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
    let uri = match driver() {
        Driver::Qemu => URI,
        Driver::Lxc => LXC_URI,
    };
    tokio::task::spawn_blocking(move || {
        let mut conn = Connect::open(Some(uri)).map_err(|e| format!("libvirt connect: {e}"))?;
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

/// Every guest on the host, VMs and containers, whichever driver `c` is.
///
/// Anything that serves guests as a set -- DHCP, the firewall, metrics, the
/// in-use check before deleting a volume, tap reattachment -- must see both,
/// or a container gets no address, no rules, and its rootfs can be deleted
/// under it. So callers never list from one connection alone.
///
/// A host with no LXC driver answers with its VMs only. The other
/// connection is closed at once; the domains keep it alive until dropped.
pub fn all_domains(c: &Connect, flags: u32) -> Vec<virt::domain::Domain> {
    let mut out = c.list_all_domains(flags).unwrap_or_default();
    let other = if c.get_type().is_ok_and(|t| t.eq_ignore_ascii_case("LXC")) {
        Some(URI)
    } else {
        lxc_available().then_some(LXC_URI)
    };
    if let Some(uri) = other
        && let Ok(mut o) = Connect::open(Some(uri))
    {
        out.extend(o.list_all_domains(flags).unwrap_or_default());
        let _ = o.close();
    }
    out
}

/// Whether the LXC driver answers. Re-checked at most once a minute: a host
/// without it would otherwise log a connection failure on every supervisor
/// pass, and a driver installed later is still picked up.
pub fn lxc_available() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static CHECKED: AtomicU64 = AtomicU64::new(0);
    static OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = CHECKED.load(Relaxed);
    if last != 0 && now.saturating_sub(last) < 60 {
        return OK.load(Relaxed);
    }
    CHECKED.store(now, Relaxed);
    let ok = match Connect::open(Some(LXC_URI)) {
        Ok(mut c) => {
            let _ = c.close();
            true
        }
        Err(_) => false,
    };
    OK.store(ok, Relaxed);
    ok
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
        let Some(rel_end) = rest[start..].find('>') else {
            break;
        };
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
            out.push(Graphics {
                kind: kind.to_string(),
                host,
                port,
            });
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

#[cfg(test)]
mod tests {
    use super::is_uuid;

    /// Against the real qemu:///system, with a throwaway network: the same
    /// object is found by UUID and by name, an unknown UUID is not found, and a
    /// network *named* like a UUID is still found by that name.
    /// `sudo -E cargo test --bins -- --ignored finds_by_uuid`
    #[test]
    #[ignore = "needs root and a reachable qemu:///system"]
    fn finds_by_uuid_and_by_name_on_real_libvirt() {
        use virt::network::Network;
        let mut c = virt::connect::Connect::open(Some(super::URI)).expect("connect");
        let weird = "00000000-0000-4000-8000-00000000abcd";
        let make = |name: &str, uuid: &str| {
            Network::define_xml(
                &c,
                &format!("<network><name>{name}</name><uuid>{uuid}</uuid></network>"),
            )
            .expect("define")
        };
        let real = "5e3c1f2a-9b8d-4c7e-a1f0-2d3b4c5d6e7f";
        let a = make("uuidprobe-a", real);
        // A network whose NAME is UUID-shaped but is not its UUID.
        let b = make(weird, "6f4d2a3b-0c9e-4d8f-b2a1-3e4c5d6e7f80");
        let got = |id: &str| super::network(&c, id).ok().and_then(|n| n.get_name().ok());
        let outcome = (
            got(real),
            got("uuidprobe-a"),
            got("7a5e3b4c-1d0f-4e9a-83b2-4f5d6e7f8091"),
            got(weird),
        );
        let _ = a.undefine();
        let _ = b.undefine();
        let _ = c.close();
        assert_eq!(outcome.0.as_deref(), Some("uuidprobe-a"), "by UUID");
        assert_eq!(outcome.1.as_deref(), Some("uuidprobe-a"), "by name");
        assert_eq!(outcome.2, None, "an unknown UUID is not found");
        assert_eq!(
            outcome.3.as_deref(),
            Some(weird),
            "a UUID-shaped name falls back to the name"
        );
    }

    #[test]
    fn only_the_canonical_uuid_shape_is_tried_as_a_uuid() {
        assert!(is_uuid("ea15249b-a713-41dc-a808-05619e42eb2a"));
        assert!(is_uuid("EA15249B-A713-41DC-A808-05619E42EB2A"));
        for not in [
            "web",
            "ea15249ba71341dca80805619e42eb2a",      // no dashes
            "ea15249b-a713-41dc-a808-05619e42eb2",   // short
            "ea15249b-a713-41dc-a808-05619e42eb2ax", // long
            "ea15249b_a713_41dc_a808_05619e42eb2a",  // wrong separator
            "zz15249b-a713-41dc-a808-05619e42eb2a",  // not hex
            "",
        ] {
            assert!(!is_uuid(not), "{not}");
        }
    }
}
