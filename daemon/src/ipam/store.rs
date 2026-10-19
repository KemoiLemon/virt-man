//! Network state: what each network is, what it hands out, and to whom.
//!
//! One JSON file (`<data_dir>/networks.json`), owned by the daemon. It is the
//! source of truth for the networks the daemon runs: their mode, subnet, DNS
//! and DHCP reservations. Nothing is read back from libvirt, which no longer
//! defines these networks at all.
//!
//! A **reservation** binds a MAC to an address for good. Ones the user set are
//! `manual`; one made because a guest is defined on the network is `dynamic`,
//! and is kept until its guest is deleted, so a guest keeps the same address
//! across reboots, lease expiry and daemon restarts. There are no expiring
//! leases to lose.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Store {
    /// Keyed by the network's UUID.
    #[serde(default)]
    pub networks: BTreeMap<String, Net>,
}

/// How a network reaches the world. The daemon owns the bridge and the
/// dnsmasq for `Nat` and `Isolated`; `Bridge` uses a bridge the operator
/// already made, which the daemon neither sets up nor deletes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Nat,
    /// A definition that lost its mode must not silently masquerade its
    /// guests onto the host's network.
    #[default]
    Isolated,
    Bridge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Net {
    /// What the user calls the network. libvirt's name is its UUID.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub mode: Mode,
    /// The operator's bridge, for `Mode::Bridge` only. A managed network's
    /// bridge is always derived from its id (`hostnet::bridge_name`), never
    /// stored and never libvirt's `virbrN`.
    #[serde(default)]
    pub bridge: String,
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// The bridge's own address: gateway, DNS server and DHCP server id.
    pub gateway: Ipv4Addr,
    pub prefix: u8,
    /// Addresses handed out to guests with no reservation (inclusive).
    pub range_start: Ipv4Addr,
    pub range_end: Ipv4Addr,
    /// Where guest DNS queries for other names go. Empty: the host's own
    /// resolvers (/etc/resolv.conf).
    #[serde(default)]
    pub dns_forwarders: Vec<Ipv4Addr>,
    /// Search domain handed to guests; guest names resolve with and without it.
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default = "default_lease")]
    pub lease_secs: u32,
    #[serde(default)]
    pub reservations: Vec<Reservation>,
}

fn default_lease() -> u32 {
    3600
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reservation {
    /// Lowercase, colon-separated.
    pub mac: String,
    pub ip: Ipv4Addr,
    /// Hostname: the guest's DHCP hostname, the name the user gave, or the
    /// guest's display name (`<title>`, falling back to libvirt's name).
    /// Answers DNS queries. A duplicate is given a numeric suffix.
    #[serde(default)]
    pub name: Option<String>,
    /// Made by the DHCP server from the range (not by a person).
    #[serde(default)]
    pub dynamic: bool,
    /// libvirt UUID of the guest owning this MAC, when known. Deleting that
    /// guest deletes the reservation.
    #[serde(default)]
    pub vm: Option<String>,
    /// Last time the guest was given this address (ms since epoch), 0 = never.
    #[serde(default)]
    pub seen_ms: u64,
}

pub fn load(path: &Path) -> Result<Store, String> {
    if !path.exists() {
        return Ok(Store::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Write atomically: a crash mid-write must not leave a truncated file, which
/// would lose every reservation at the next start. The file names every guest's
/// address and MAC, so it is created 0600 and renamed into place.
pub fn save(path: &Path, store: &Store) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize ipam: {e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    // An existing temp file keeps its old mode through `open`.
    let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    f.write_all(&text)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

// ------------------------------------------------------------ addressing

pub fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

impl Net {
    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(mask(self.prefix))
    }
    /// A network whose bridge, addressing and dhcp are the daemon's. A bridge
    /// network only borrows an interface the operator owns.
    pub fn is_managed(&self) -> bool {
        self.mode != Mode::Bridge
    }
    /// The network address, e.g. `192.168.1.0/24`. None when there is no
    /// address of our own to describe: a bridge network, or a gateway of
    /// 0.0.0.0 (a placeholder definition that has not been given a subnet).
    pub fn subnet(&self) -> Option<String> {
        if !self.is_managed() || self.gateway.is_unspecified() {
            return None;
        }
        Some(format!(
            "{}/{}",
            Ipv4Addr::from(self.network()),
            self.prefix
        ))
    }
    /// The bridge address with its prefix length, e.g. `192.168.1.1/24`.
    pub fn cidr(&self) -> Option<String> {
        if !self.is_managed() || self.gateway.is_unspecified() {
            return None;
        }
        Some(format!("{}/{}", self.gateway, self.prefix))
    }
    /// Whether dnsmasq has something to serve: a managed bridge with a subnet.
    pub fn is_served(&self) -> bool {
        self.is_managed() && self.subnet().is_some()
    }
    fn network(&self) -> u32 {
        u32::from(self.gateway) & mask(self.prefix)
    }
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.network() | !mask(self.prefix))
    }
    /// Inside the subnet, and neither its network nor broadcast address.
    pub fn contains_host(&self, ip: Ipv4Addr) -> bool {
        let n = u32::from(ip);
        n & mask(self.prefix) == self.network()
            && n != self.network()
            && n != u32::from(self.broadcast())
    }
    pub fn reservation_for_mac(&self, mac: &str) -> Option<&Reservation> {
        self.reservations.iter().find(|r| r.mac == mac)
    }
    pub fn reservation_for_ip(&self, ip: Ipv4Addr) -> Option<&Reservation> {
        self.reservations.iter().find(|r| r.ip == ip)
    }
    /// The lowest address in the range that is not the gateway and not
    /// reserved. `taken` are addresses seen in use on the wire (answered ARP /
    /// ICMP), so a statically configured guest is not handed out twice.
    pub fn next_free(&self, taken: &dyn Fn(Ipv4Addr) -> bool) -> Option<Ipv4Addr> {
        let (a, b) = (u32::from(self.range_start), u32::from(self.range_end));
        (a..=b).map(Ipv4Addr::from).find(|&ip| {
            ip != self.gateway
                && self.contains_host(ip)
                && self.reservation_for_ip(ip).is_none()
                && !taken(ip)
        })
    }
    /// Check a range against the subnet: both ends inside it, in order.
    pub fn validate_range(&self, start: Ipv4Addr, end: Ipv4Addr) -> Result<(), String> {
        if !self.contains_host(start) || !self.contains_host(end) {
            return Err(format!(
                "the range must lie inside {}/{}",
                Ipv4Addr::from(self.network()),
                self.prefix
            ));
        }
        if u32::from(start) > u32::from(end) {
            return Err("the range ends before it starts".into());
        }
        Ok(())
    }

    /// Fill the range with the subnet's usable addresses after the gateway:
    /// `gateway+1` .. broadcast-1, never including the gateway itself. Used
    /// when a network is given a subnet the user did not write a range for. A
    /// gateway at the top of the subnet wraps the start to the first host, so
    /// the range is never empty or the broadcast.
    pub fn default_range(&mut self) {
        if self.gateway.is_unspecified() {
            return;
        }
        let network = self.network();
        let broadcast = u32::from(self.broadcast());
        let gateway = u32::from(self.gateway);
        let mut start = gateway.saturating_add(1);
        if start >= broadcast {
            start = network + 1;
        }
        if start == gateway {
            start += 1;
        }
        let mut end = broadcast.saturating_sub(1);
        if end == gateway {
            end = gateway.saturating_sub(1);
        }
        self.range_start = Ipv4Addr::from(start.min(end));
        self.range_end = Ipv4Addr::from(end);
    }

    /// Make sure this network has a permanent reservation for `mac`, owned by
    /// guest `vm`, named after its display name. The lowest free address in
    /// the range is taken, so a guest that appears on a network gets an
    /// address without asking for one. Returns whether one was inserted; an
    /// existing reservation keeps its address and name, and only gains its
    /// owner if it had none.
    pub fn ensure_reservation(
        &mut self,
        mac: &str,
        vm: &str,
        display: &str,
    ) -> Result<bool, String> {
        let mac = normalize_mac(mac).ok_or_else(|| format!("'{mac}' is not a MAC address"))?;
        if let Some(r) = self.reservations.iter_mut().find(|r| r.mac == mac) {
            if r.vm.is_none() {
                r.vm = Some(vm.to_string());
            }
            return Ok(false);
        }
        let base = clean_hostname(display).unwrap_or_else(|| "guest".into());
        let name = self.unique_name(&base, &mac);
        let ip = self.next_free(&|_| false).ok_or_else(|| {
            format!(
                "no free address left in {}..{}",
                self.range_start, self.range_end
            )
        })?;
        self.reservations.push(Reservation {
            mac,
            ip,
            name: Some(name),
            dynamic: true,
            vm: Some(vm.to_string()),
            seen_ms: 0,
        });
        Ok(true)
    }

    /// A hostname not already taken by another guest's reservation on this
    /// network: `base`, or `base-2`, `base-3`, … Two guests may share a
    /// display name (they are named by UUID), but two reservations in one
    /// network must not answer DNS as the same name, or one guest would
    /// resolve to the other's address. `mac`'s own reservation is not a clash,
    /// so re-deriving a name is stable.
    pub fn unique_name(&self, base: &str, mac: &str) -> String {
        let taken = |n: &str| {
            self.reservations
                .iter()
                .any(|r| r.mac != mac && r.name.as_deref() == Some(n))
        };
        if !taken(base) {
            return base.to_string();
        }
        for i in 2..1000u32 {
            let suffix = format!("-{i}");
            let keep = 63usize.saturating_sub(suffix.len());
            // Truncate the base because a hostname is capped at 63 characters,
            // and the suffix is what makes it unique.
            let cand = format!("{}{suffix}", &base[..base.len().min(keep)]);
            if !taken(&cand) {
                return cand;
            }
        }
        base.to_string()
    }
}

/// Choose the `/24` a new network should use, avoiding every prefix in
/// `used`.
///
/// Tries `10.0.0.0/8` first, then `192.168.0.0/16`, as requested. Within each
/// pool the address is random rather than sequential, so two hosts allocating
/// the same subnet at the same time are unlikely to agree, and a deleted
/// network's address is not immediately handed to the next one (which would
/// make a stale guest config point at a stranger).
///
/// The first octet of each pool is skipped where the host or real life is
/// likely to have it already: `10.0.x` and `10.1.x` are the usual VPN ranges
/// (WireGuard, Tailscale, some corporate tunnels), and `192.168.0/1/2` plus
/// `.100` and `.122` are the common home-router and VirtualBox defaults. This
/// is a heuristic, not a guarantee -- `used` is what actually decides.
/// `used` should hold every prefix the host can already reach (its own
/// interfaces, its routes, and every defined network), as `(u32, u8)` network
/// address and prefix length. A candidate collides if it overlaps any of them
/// in either direction: a /16 in `used` swallows every /24 inside it, and a
/// used /24 inside a candidate /16 counts too, so a network the host is on is
/// never handed out.
///
/// Returns `None` only if both pools are exhausted, which on a real host means
/// something is very wrong and the caller must fail rather than guess.
pub fn pick_subnet(used: &[(u32, u8)], rand: &mut dyn FnMut() -> u32) -> Option<Ipv4Addr> {
    // Each pool is every /24 inside it: 10/8 has 65536 of them, 192.168/16 has
    // 256. `base` is the pool's network address and `count` how many /24s it
    // holds, so candidate i is `base + (i << 8)`.
    const POOLS: [(u32, u32); 2] = [
        (0x0A00_0000, 1 << 16), // 10.0.0.0/8
        (0xC0A8_0000, 1 << 8),  // 192.168.0.0/16
    ];
    // Ranges skipped because real hosts very often already have them: 10.0/16
    // and 10.1/16 (WireGuard, Tailscale, corporate VPNs) and 192.168.0/1/2
    // plus .100 and .122 (home routers, VirtualBox defaults). A heuristic, not
    // a guarantee -- `used` is what actually decides.
    let skip = |base: u32, cand: u32| -> bool {
        let off = cand - base;
        match base {
            0x0A00_0000 => off < (2 << 16),
            0xC0A8_0000 => (off >> 8) <= 2 || (off >> 8) == 100 || (off >> 8) == 122,
            _ => false,
        }
    };
    for (base, count) in POOLS {
        // Start at a random /24 in the pool. Stepping through a pool from a
        // random point and wrapping made the walk appear to exhaust a pool
        // that still had most of its space free, and fall through to 192.168
        // with 10/8 wide open, so the walk covers the whole pool first.
        let start = rand() % count;
        for i in 0..count {
            let cand = base + (((start + i) % count) << 8);
            if skip(base, cand) || overlaps_any(cand, 24, used) {
                continue;
            }
            return Some(Ipv4Addr::from(cand));
        }
    }
    None
}

/// Whether `cand/prefix` overlaps any used prefix. Overlap is symmetric: the
/// candidate may sit inside a used prefix, or contain one. Also used by the
/// network API to refuse a hand-typed subnet that collides.
pub fn overlaps_any(cand: u32, prefix: u8, used: &[(u32, u8)]) -> bool {
    used.iter().any(|&(net, p)| {
        let p = p.min(32);
        let same = |a: u32, pa: u8, b: u32, pb: u8| {
            let m = mask(pa.min(pb));
            (a & m) == (b & m)
        };
        same(cand, prefix, net, p)
    })
}

/// `aa:BB:cc…` or `aa-bb-…` to lowercase colon form; None if not a MAC.
pub fn normalize_mac(mac: &str) -> Option<String> {
    let parts: Vec<&str> = mac.trim().split([':', '-']).collect();
    if parts.len() != 6
        || parts
            .iter()
            .any(|p| p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(parts.join(":").to_ascii_lowercase())
}

/// A hostname safe to answer DNS for: letters, digits and hyphens, not
/// starting or ending with a hyphen, at most 63 characters. Anything else is
/// cleaned rather than rejected, since it usually comes from a guest.
pub fn clean_hostname(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-');
    let s = &s[..s.len().min(63)];
    let s = s.trim_end_matches('-');
    (!s.is_empty()).then(|| s.to_string())
}

/// A DNS domain this host may serve: lowercase labels of letters, digits and
/// hyphens separated by single dots. It is written into a dnsmasq config file,
/// which is read line by line, so whitespace or a newline must never get in.
/// Anything not valid is rejected rather than cleaned: this is the user's
/// input, not a guest's.
pub fn valid_domain(raw: &str) -> Option<String> {
    let d = raw.trim().to_ascii_lowercase();
    (!d.is_empty()
        && d.split('.')
            .all(|l| clean_hostname(l).as_deref() == Some(l)))
    .then_some(d)
}

// ------------------------------------------------------------ XML import

/// Read a network definition — libvirt's shape, which is also what the SPA
/// editor speaks — into the stored state. Every supported mode imports,
/// including one with no subnet of its own: a bridge network, or an isolated
/// segment nothing serves. Returns an error for a mode this daemon cannot
/// reproduce (`route`, `open`, `passthrough`) rather than silently forcing it
/// into isolation.
pub fn import(xml: &str) -> Result<Net, String> {
    // Modes we can reproduce natively. libvirt's `route`, `open`, `passthrough`
    // and anything newer have no native equivalent here yet, so such a
    // definition is refused rather than silently forced into isolation.
    let mode = match open_tag(xml, "<forward ").and_then(|t| attr(t, "mode")) {
        Some("nat") => Mode::Nat,
        Some("bridge") => Mode::Bridge,
        Some("none") | Some("") => Mode::Isolated,
        None => Mode::Isolated,
        Some(other) => return Err(format!("forward mode '{other}' is not supported")),
    };
    // Only an operator's bridge is stored: a managed network derives its own,
    // and libvirt's `virbrN` is libvirt's.
    let bridge = if mode == Mode::Bridge {
        let b = open_tag(xml, "<bridge ")
            .and_then(|t| attr(t, "name"))
            .unwrap_or_default()
            .to_string();
        if b.is_empty() {
            return Err("a bridge network must name its bridge".into());
        }
        b
    } else {
        String::new()
    };
    // An <ip> of another family describes nothing this daemon serves.
    let ip_tag = open_tag(xml, "<ip ").filter(|t| !attr(t, "family").is_some_and(|f| f != "ipv4"));
    let mut net = Net {
        title: element_text(xml, "title").unwrap_or_default(),
        mode,
        bridge,
        // Autostart is a separate property in libvirt, not in the XML. A
        // network this daemon defines starts on boot unless told otherwise.
        autostart: true,
        gateway: Ipv4Addr::UNSPECIFIED,
        prefix: 0,
        range_start: Ipv4Addr::UNSPECIFIED,
        range_end: Ipv4Addr::UNSPECIFIED,
        dns_forwarders: Vec::new(),
        domain: open_tag(xml, "<domain ")
            .and_then(|t| attr(t, "name"))
            .and_then(valid_domain),
        lease_secs: default_lease(),
        reservations: Vec::new(),
    };
    // A bridge network borrows the operator's bridge and its addressing, and
    // an isolated segment may deliberately have no subnet. Both are definitions
    // with nothing of our own to serve.
    if mode == Mode::Bridge {
        return Ok(net);
    }
    let Some(ip_tag) = ip_tag else { return Ok(net) };
    let Some(gateway) = attr(ip_tag, "address") else {
        return Ok(net);
    };
    net.gateway = gateway
        .parse()
        .map_err(|_| format!("'{gateway}' is not an IPv4 address"))?;
    net.prefix = match (attr(ip_tag, "prefix"), attr(ip_tag, "netmask")) {
        (Some(p), _) => p
            .parse()
            .map_err(|_| format!("'{p}' is not a prefix length"))?,
        (None, Some(m)) => u32::from(
            m.parse::<Ipv4Addr>()
                .map_err(|_| format!("'{m}' is not a netmask"))?,
        )
        .count_ones() as u8,
        _ => 24,
    };
    // Default range: the whole subnet after the gateway.
    net.default_range();
    if let Some(r) = open_tag(xml, "<range ")
        && let (Some(s), Some(e)) = (attr(r, "start"), attr(r, "end"))
        && let (Ok(s), Ok(e)) = (s.parse(), e.parse())
    {
        net.range_start = s;
        net.range_end = e;
    }
    for f in all_tags(xml, "<forwarder ") {
        if let Some(Ok(a)) = attr(f, "addr").map(str::parse) {
            net.dns_forwarders.push(a);
        }
    }
    for h in all_tags(xml, "<host ") {
        let (Some(mac), Some(ip)) = (attr(h, "mac").and_then(normalize_mac), attr(h, "ip")) else {
            continue;
        };
        let Ok(ip) = ip.parse() else { continue };
        if net.reservation_for_mac(&mac).is_none() && net.reservation_for_ip(ip).is_none() {
            net.reservations.push(Reservation {
                mac,
                ip,
                name: attr(h, "name").and_then(clean_hostname),
                dynamic: false,
                vm: None,
                seen_ms: 0,
            });
        }
    }
    Ok(net)
}

fn open_tag<'a>(xml: &'a str, start: &str) -> Option<&'a str> {
    let i = xml.find(start)?;
    let e = xml[i..].find('>')?;
    Some(&xml[i..=i + e])
}

/// The text of the first `<name>…</name>` element, trimmed and unescaped.
fn element_text(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let i = xml.find(&open)? + open.len();
    let end = xml[i..].find(&close)?;
    Some(crate::libvirt::unescape(xml[i..i + end].trim()))
}

fn all_tags<'a>(xml: &'a str, start: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(start) {
        let Some(e) = rest[i..].find('>') else { break };
        out.push(&rest[i..=i + e]);
        rest = &rest[i + e + 1..];
    }
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    crate::disks::attr(tag, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBVIRT: &str = "<network><name>internet</name><forward mode='nat'/>
      <dns><forwarder addr='1.1.1.1'/><forwarder addr='9.9.9.9'/></dns>
      <ip address='192.168.167.1' netmask='255.255.255.0'>
        <dhcp>
          <range start='192.168.167.100' end='192.168.167.199'/>
          <host mac='52:54:00:82:DE:19' name='alpine' ip='192.168.167.2'/>
          <host mac='52:54:00:00:00:02' ip='192.168.167.3'/>
          <host mac='52:54:00:00:00:02' ip='192.168.167.4'/>
        </dhcp>
      </ip></network>";

    fn net() -> Net {
        import(LIBVIRT).unwrap()
    }

    #[test]
    fn a_libvirt_network_is_imported_with_its_range_forwarders_and_hosts() {
        let n = net();
        assert_eq!(
            (n.gateway, n.prefix),
            ("192.168.167.1".parse().unwrap(), 24)
        );
        assert_eq!(
            n.range_start,
            "192.168.167.100".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(n.range_end, "192.168.167.199".parse::<Ipv4Addr>().unwrap());
        assert_eq!(n.dns_forwarders.len(), 2);
        assert_eq!(n.reservations.len(), 2, "a duplicate MAC is imported once");
        let a = n.reservation_for_mac("52:54:00:82:de:19").unwrap();
        assert_eq!(
            (a.ip, a.name.as_deref(), a.dynamic),
            ("192.168.167.2".parse().unwrap(), Some("alpine"), false)
        );
    }

    #[test]
    fn old_json_gains_the_new_fields_with_their_defaults() {
        let old = r#"{"networks":{"internet":{"gateway":"10.0.0.1","prefix":24,"range_start":"10.0.0.2","range_end":"10.0.0.254"}}}"#;
        let s: Store = serde_json::from_str(old).unwrap();
        let n = &s.networks["internet"];
        assert_eq!(
            (n.title.as_str(), n.mode, n.bridge.as_str(), n.autostart),
            ("", Mode::Isolated, "", true),
            "a missing mode is isolated, never an implicit masquerade"
        );
    }

    #[test]
    fn an_unsupported_forward_mode_is_refused() {
        // `route`, `open` and `passthrough` have no native equivalent yet.
        for mode in ["route", "open", "passthrough", "future"] {
            let xml = format!(
                "<network><forward mode='{mode}'/><ip address='10.1.0.1' prefix='24'/></network>"
            );
            assert!(import(&xml).is_err(), "{mode}");
        }
        // The modes we do implement still map.
        assert_eq!(
            import("<network><forward mode='none'/><ip address='10.1.0.1' prefix='24'/></network>")
                .unwrap()
                .mode,
            Mode::Isolated
        );
        assert_eq!(
            import("<network><forward mode=''/><ip address='10.1.0.1' prefix='24'/></network>")
                .unwrap()
                .mode,
            Mode::Isolated
        );
    }

    #[test]
    fn libvirt_forwarding_mode_bridge_and_title_are_imported() {
        let x = "<network><title>Lab</title><forward mode='none'/><bridge name='br-lan'/><ip address='10.1.0.1' prefix='24'/></network>";
        let n = import(x).unwrap();
        assert_eq!(
            (n.mode, n.bridge.as_str(), n.title.as_str()),
            (Mode::Isolated, "", "Lab"),
            "a managed network never keeps libvirt's bridge"
        );
        // Only an operator's bridge is stored as one.
        let b = import(
            "<network><forward mode='bridge'/><bridge name='br-lan'/><ip address='10.1.0.1' prefix='24'/></network>",
        )
        .unwrap();
        assert_eq!((b.mode, b.bridge.as_str()), (Mode::Bridge, "br-lan"));
        let nat =
            import("<network><forward mode='nat'/><ip address='10.1.0.1' prefix='24'/></network>")
                .unwrap();
        assert_eq!(nat.mode, Mode::Nat);
        // libvirt forwards nothing without a <forward> element.
        let none = import("<network><ip address='10.1.0.1' prefix='24'/></network>").unwrap();
        assert_eq!(none.mode, Mode::Isolated);
        assert!(none.autostart);
        // libvirt keeps autostart outside the XML; there is nothing to import.
        assert!(
            import("<network><ip address='10.1.0.1' prefix='24'/></network>")
                .unwrap()
                .autostart
        );
    }

    #[test]
    fn modes_round_trip_through_json() {
        for (mode, text) in [
            (Mode::Nat, "\"nat\""),
            (Mode::Isolated, "\"isolated\""),
            (Mode::Bridge, "\"bridge\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), text);
            assert_eq!(serde_json::from_str::<Mode>(text).unwrap(), mode);
        }
        let n = Net {
            mode: Mode::Bridge,
            bridge: "br0".into(),
            ..net()
        };
        assert_eq!(
            serde_json::from_str::<Net>(&serde_json::to_string(&n).unwrap()).unwrap(),
            n,
            "a bridge network keeps its mode and operator bridge"
        );
    }

    #[test]
    fn a_domain_must_be_labels_and_never_reaches_a_config_line() {
        assert_eq!(
            valid_domain("  LAB.Example ").as_deref(),
            Some("lab.example")
        );
        assert_eq!(valid_domain("lab").as_deref(), Some("lab"));
        for bad in [
            "",
            " ",
            "a b",
            "lab\nlisten-address=0.0.0.0",
            "lab\tlisten-address=0.0.0.0",
            "a..b",
            ".lab",
            "lab.",
            "_dmarc.example",
        ] {
            assert_eq!(valid_domain(bad), None, "{bad:?}");
        }
        // import() drops an invalid domain instead of storing it.
        let n = import(
            "<network><domain name='lab\nlisten-address=0.0.0.0'/><ip address='10.1.0.1' prefix='24'/></network>",
        )
        .unwrap();
        assert_eq!(n.domain, None);
    }

    #[test]
    fn with_no_range_the_whole_subnet_after_the_gateway_is_dynamic() {
        let n = import("<network><ip address='10.0.0.1' prefix='24'/></network>").unwrap();
        assert_eq!(
            (n.range_start, n.range_end),
            ("10.0.0.2".parse().unwrap(), "10.0.0.254".parse().unwrap())
        );
        let n = import("<network><ip address='10.0.0.254' netmask='255.255.255.0'/></network>")
            .unwrap();
        assert_eq!(
            n.range_start,
            "10.0.0.1".parse::<Ipv4Addr>().unwrap(),
            "gateway not at .1"
        );
    }

    #[test]
    fn a_definition_without_a_subnet_still_imports() {
        // An operator's bridge is borrowed, so it names one and has no subnet.
        let b = import("<network><forward mode='bridge'/><bridge name='br0'/></network>").unwrap();
        assert_eq!((b.mode, b.bridge.as_str()), (Mode::Bridge, "br0"));
        assert!(b.subnet().is_none() && b.gateway.is_unspecified());
        assert!(b.range_start.is_unspecified() && b.range_end.is_unspecified());
        assert!(import("<network><forward mode='bridge'/></network>").is_err());

        // An isolated segment may have no subnet of its own.
        let i = import("<network><forward mode='none'/></network>").unwrap();
        assert_eq!(i.mode, Mode::Isolated);
        assert!(i.subnet().is_none() && !i.is_served());

        // A NAT network with no <ip>: the API fills a generated /24.
        let n = import("<network><forward mode='nat'/></network>").unwrap();
        assert_eq!(n.mode, Mode::Nat);
        assert!(n.subnet().is_none() && n.gateway.is_unspecified());

        // An IPv6 <ip> is not ours to serve either.
        let v6 =
            import("<network><ip family='ipv6' address='fd00::1' prefix='64'/></network>").unwrap();
        assert!(v6.gateway.is_unspecified() && !v6.is_served());
    }

    #[test]
    fn a_generated_subnet_gets_the_subnet_after_its_gateway() {
        let mut n = import("<network><forward mode='nat'/></network>").unwrap();
        n.gateway = "10.42.7.1".parse().unwrap();
        n.prefix = 24;
        n.default_range();
        assert_eq!(
            (n.range_start, n.range_end),
            ("10.42.7.2".parse().unwrap(), "10.42.7.254".parse().unwrap())
        );
        // A gateway at the top wraps to the first host instead of the broadcast.
        let mut n = import("<network><forward mode='nat'/></network>").unwrap();
        n.gateway = "10.42.7.254".parse().unwrap();
        n.prefix = 24;
        n.default_range();
        assert_eq!(n.range_start, "10.42.7.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(
            n.range_end,
            "10.42.7.253".parse::<Ipv4Addr>().unwrap(),
            "the gateway itself is never in the range"
        );
    }

    #[test]
    fn a_guest_appearing_on_a_network_gets_the_lowest_free_address() {
        let mut n = import("<network><ip address='10.9.0.1' prefix='24'/></network>").unwrap();
        assert!(
            n.ensure_reservation("52-54-00-AA-00-01", "u1", "My Web")
                .unwrap()
        );
        assert_eq!(n.reservations.len(), 1);
        let r = &n.reservations[0];
        assert_eq!(r.mac, "52:54:00:aa:00:01");
        assert_eq!(r.ip, "10.9.0.2".parse::<Ipv4Addr>().unwrap());
        assert_eq!(r.name.as_deref(), Some("my-web"));
        assert!(r.dynamic && r.vm.as_deref() == Some("u1") && r.seen_ms == 0);

        // A second guest takes the next one; a duplicate display name is numbered.
        assert!(
            n.ensure_reservation("52:54:00:aa:00:02", "u2", "My Web")
                .unwrap()
        );
        assert_eq!(
            n.reservations[1].ip,
            "10.9.0.3".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(n.reservations[1].name.as_deref(), Some("my-web-2"));

        // The same MAC is not inserted twice, and a missing owner is filled in.
        let mut manual = n.reservations[0].clone();
        manual.vm = None;
        n.reservations[0] = manual;
        assert!(
            !n.ensure_reservation("52:54:00:aa:00:01", "u1", "Other")
                .unwrap()
        );
        assert_eq!(n.reservations.len(), 2);
        assert_eq!(
            n.reservations[0].ip,
            "10.9.0.2".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(n.reservations[0].name.as_deref(), Some("my-web"));
        assert_eq!(n.reservations[0].vm.as_deref(), Some("u1"));

        // A full range is an error, not a wrong address.
        let mut full = import("<network><ip address='10.9.0.1' prefix='30'/></network>").unwrap();
        assert!(
            full.ensure_reservation("52:54:00:aa:00:01", "u1", "a")
                .unwrap()
        );
        assert_eq!(
            full.reservations[0].ip,
            "10.9.0.2".parse::<Ipv4Addr>().unwrap()
        );
        assert!(
            full.ensure_reservation("52:54:00:aa:00:02", "u2", "b")
                .is_err()
        );
    }

    #[test]
    fn the_allocator_skips_the_gateway_reservations_and_addresses_in_use() {
        let mut n = import("<network><ip address='10.0.0.1' prefix='29'/></network>").unwrap();
        // .1 gateway, .2-.6 hosts, .7 broadcast
        n.reservations.push(Reservation {
            mac: "m".into(),
            ip: "10.0.0.2".parse().unwrap(),
            name: None,
            dynamic: true,
            vm: None,
            seen_ms: 0,
        });
        let busy = |ip: Ipv4Addr| ip == "10.0.0.3".parse::<Ipv4Addr>().unwrap();
        assert_eq!(n.next_free(&busy), Some("10.0.0.4".parse().unwrap()));
        let all = |_: Ipv4Addr| true;
        assert_eq!(
            n.next_free(&all),
            None,
            "a full range answers None, not a wrong address"
        );
    }

    #[test]
    fn ranges_must_be_inside_the_subnet_and_in_order() {
        let n = net();
        assert!(
            n.validate_range(
                "192.168.167.10".parse().unwrap(),
                "192.168.167.20".parse().unwrap()
            )
            .is_ok()
        );
        assert!(
            n.validate_range(
                "192.168.168.10".parse().unwrap(),
                "192.168.167.20".parse().unwrap()
            )
            .is_err()
        );
        assert!(
            n.validate_range(
                "192.168.167.20".parse().unwrap(),
                "192.168.167.10".parse().unwrap()
            )
            .is_err()
        );
        assert!(
            n.validate_range(
                "192.168.167.0".parse().unwrap(),
                "192.168.167.9".parse().unwrap()
            )
            .is_err(),
            "network address"
        );
        assert!(
            n.validate_range(
                "192.168.167.9".parse().unwrap(),
                "192.168.167.255".parse().unwrap()
            )
            .is_err(),
            "broadcast"
        );
    }

    #[test]
    fn a_duplicate_hostname_gets_a_number() {
        let mut n = net();
        // A guest is named by its UUID, so two guests may share a display
        // name; two reservations in one network must still answer DNS apart,
        // or one guest would resolve to the other's address.
        n.reservations.push(Reservation {
            mac: "52:54:00:00:00:99".into(),
            ip: "192.168.167.9".parse().unwrap(),
            name: Some("alpine".into()),
            dynamic: true,
            vm: None,
            seen_ms: 0,
        });
        // A name nobody holds is kept as it is, even a maximal one.
        assert_eq!(n.unique_name("fresh", "52:54:00:00:00:42"), "fresh");
        assert_eq!(
            n.unique_name(&"z".repeat(63), "52:54:00:00:00:42"),
            "z".repeat(63)
        );
        // "alpine" is held by another MAC, so this one is numbered.
        assert_eq!(n.unique_name("alpine", "52:54:00:00:00:42"), "alpine-2");
        // The suffix is what makes it unique, so a taken 63-character base is
        // truncated to leave room rather than exceeding the hostname limit.
        n.reservations.push(Reservation {
            mac: "52:54:00:00:00:98".into(),
            ip: "192.168.167.8".parse().unwrap(),
            name: Some("a".repeat(63)),
            dynamic: true,
            vm: None,
            seen_ms: 0,
        });
        let long = n.unique_name(&"a".repeat(63), "52:54:00:00:00:42");
        assert_eq!(long, format!("{}-2", "a".repeat(61)));
        assert_eq!(long.len(), 63);
    }

    #[test]
    fn a_hostname_is_never_its_own_clash() {
        let n = net();
        // Re-deriving a reservation's own name is stable: the reservation
        // holding it is not a clash with itself.
        assert_eq!(n.unique_name("alpine", "52:54:00:82:de:19"), "alpine");
    }

    #[test]
    fn macs_and_hostnames_are_normalised() {
        assert_eq!(
            normalize_mac("52-54-00-AB-cd-EF").as_deref(),
            Some("52:54:00:ab:cd:ef")
        );
        assert!(normalize_mac("52:54:00:ab:cd").is_none());
        assert!(normalize_mac("zz:54:00:ab:cd:ef").is_none());
        assert_eq!(
            clean_hostname("My VM_01.local").as_deref(),
            Some("my-vm-01-local")
        );
        assert_eq!(clean_hostname("--").as_deref(), None);
        assert_eq!(clean_hostname(&"a".repeat(80)).unwrap().len(), 63);
    }

    #[test]
    fn the_store_round_trips_through_its_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ipam-test-{}", std::process::id()));
        let path = dir.join("networks.json");
        let mut s = Store::default();
        s.networks.insert("internet".into(), net());
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), s);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left behind"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the store names every guest's address and MAC"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A fixed "random" so the walk from the random offset is deterministic.
    fn seq(start: u32) -> impl FnMut() -> u32 {
        let mut n = start;
        move || {
            n = n.wrapping_add(1);
            n
        }
    }

    #[test]
    fn a_generated_subnet_is_a_24_in_10_or_192_168_and_avoids_the_host() {
        // A host on 10.20.30.0/24 (its own LAN) and already serving
        // 10.40.0.0/24, plus a 192.168.5.0/24.
        let used = [
            (u32::from(Ipv4Addr::new(10, 20, 30, 0)), 24),
            (u32::from(Ipv4Addr::new(10, 40, 0, 0)), 24),
            (u32::from(Ipv4Addr::new(192, 168, 5, 0)), 24),
        ];
        for seed in 0..300u32 {
            let ip = pick_subnet(&used, &mut seq(seed)).expect("a subnet is always available");
            let n = u32::from(ip);
            assert_eq!(ip.octets()[3], 0, "{ip} must be a /24 network address");
            let a = ip.octets()[0];
            assert!(a == 10 || (a == 192 && ip.octets()[1] == 168), "{ip}");
            assert!(
                !overlaps_any(n, 24, &used),
                "{ip} overlaps something already in use"
            );
        }
    }

    /// The heuristic skips the ranges a real host most often already has, and
    /// falls through to 192.168 only once 10/8 is genuinely full.
    #[test]
    fn the_common_ranges_are_skipped_and_192_168_is_the_fallback() {
        // Nothing used: a generated value must avoid the reserved offsets, so a
        // host's real 10.0.x VPN is not collided with, and must prefer 10/8
        // (192.168 only when 10/8 cannot serve).
        for seed in 0..64u32 {
            let ip = pick_subnet(&[], &mut seq(seed)).unwrap();
            let o = ip.octets();
            assert!(
                (o[0] == 10 && o[1] > 1)
                    || (o[0] == 192 && o[1] == 168 && o[2] > 2 && o[2] != 100 && o[2] != 122),
                "{ip} is in a reserved range"
            );
        }
        // With 10.0/16 and 10.1/16 reserved but the rest free, the answer must
        // come from 10/8 rather than 192.168: the pool order is the point.
        let ip = pick_subnet(&[], &mut seq(3)).unwrap();
        assert_eq!(ip.octets()[0], 10, "10/8 is used before 192.168: {ip}");
        assert!(ip.octets()[1] > 1, "10.0/16 and 10.1/16 are reserved: {ip}");
        // Mark every /24 of 10/8 used: the answer must move to 192.168.
        let used: Vec<(u32, u8)> = (0..(1u32 << 16))
            .map(|b| (0x0A00_0000 + (b << 8), 24))
            .collect();
        let ip = pick_subnet(&used, &mut seq(7)).unwrap();
        assert_eq!(&ip.octets()[..2], &[192, 168], "fell back to 192.168: {ip}");
        assert!(
            ip.octets()[2] > 2 && ip.octets()[2] != 100 && ip.octets()[2] != 122,
            "192.168.0/1/2 and .100/.122 are reserved: {ip}"
        );
    }

    /// A used prefix wider than /24 swallows the candidates inside it, and a
    /// candidate is not handed out if it would contain a used one.
    #[test]
    fn overlap_is_checked_in_both_directions() {
        // The host is on 10.50.0.0/16: every 10.50.x.0/24 is out.
        let used = [(u32::from(Ipv4Addr::new(10, 50, 0, 0)), 16)];
        assert!(overlaps_any(
            u32::from(Ipv4Addr::new(10, 50, 7, 0)),
            24,
            &used
        ));
        assert!(!overlaps_any(
            u32::from(Ipv4Addr::new(10, 51, 0, 0)),
            24,
            &used
        ));
        for _ in 0..50 {
            let ip = pick_subnet(&used, &mut seq(3)).unwrap();
            assert_ne!(ip.octets()[1], 50, "never inside the host's /16: {ip}");
        }
    }

    #[test]
    fn both_pools_exhausted_returns_none() {
        let mut used: Vec<(u32, u8)> = (0..(1u32 << 16))
            .map(|b| (0x0A00_0000 + (b << 8), 24))
            .collect();
        used.extend((0..256u32).map(|b| (0xC0A8_0000 + (b << 8), 24)));
        assert!(
            pick_subnet(&used, &mut seq(0)).is_none(),
            "nothing left to hand out must be reported, not guessed"
        );
    }
}
