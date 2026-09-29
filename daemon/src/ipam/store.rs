//! IPAM state: what each managed network hands out, and to whom.
//!
//! One JSON file (`/etc/virt-man/ipam.json`), owned by the daemon. It replaces
//! the `<dhcp>` section libvirt used to keep in the network definition: editing
//! a reservation there meant `virNetworkUpdate` or a redefine, and a redefine
//! restarts the network and unplugs every guest on it.
//!
//! A **reservation** binds a MAC to an address for good. Ones the user set are
//! `manual`; one made because a guest asked for an address and got one from
//! the range is `dynamic`, and is kept until its guest is deleted, so a guest
//! keeps the same address across reboots, lease expiry and daemon restarts.
//! There are no expiring leases to lose.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;

pub const IPAM_PATH: &str = "/etc/virt-man/ipam.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Store {
    /// Keyed by libvirt network name.
    #[serde(default)]
    pub networks: BTreeMap<String, Net>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Net {
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
/// would lose every reservation at the next start.
pub fn save(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize ipam: {e}"))?;
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
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

// ------------------------------------------------------------ libvirt import

/// Read what libvirt's `<ip>` (with its `<dhcp>` and `<dns>`) describes, so a
/// network taken over from dnsmasq keeps every address it had. Returns None
/// for a network without an IPv4 subnet (bridge mode, or isolated with no
/// `<ip>`): there is nothing for us to serve there.
pub fn import(xml: &str) -> Option<Net> {
    let ip_tag = open_tag(xml, "<ip ")?;
    if attr(ip_tag, "family").is_some_and(|f| f != "ipv4") {
        return None;
    }
    let gateway: Ipv4Addr = attr(ip_tag, "address")?.parse().ok()?;
    let prefix = match (attr(ip_tag, "prefix"), attr(ip_tag, "netmask")) {
        (Some(p), _) => p.parse().ok()?,
        (None, Some(m)) => u32::from(m.parse::<Ipv4Addr>().ok()?).count_ones() as u8,
        _ => 24,
    };
    let mut net = Net {
        gateway,
        prefix,
        range_start: gateway,
        range_end: gateway,
        dns_forwarders: Vec::new(),
        domain: open_tag(xml, "<domain ")
            .and_then(|t| attr(t, "name"))
            .map(str::to_string),
        lease_secs: default_lease(),
        reservations: Vec::new(),
    };
    // Default range: the whole subnet after the gateway.
    let first = Ipv4Addr::from((u32::from(gateway) & mask(prefix)) + 1);
    let last = Ipv4Addr::from(u32::from(net.broadcast()) - 1);
    net.range_start = if first == gateway {
        Ipv4Addr::from(u32::from(gateway) + 1)
    } else {
        first
    };
    net.range_end = last;
    if let Some(r) = open_tag(xml, "<range ")
        && let (Some(s), Some(e)) = (attr(r, "start"), attr(r, "end"))
        && let (Ok(s), Ok(e)) = (s.parse(), e.parse())
        && net.validate_range(s, e).is_ok()
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
    Some(net)
}

fn open_tag<'a>(xml: &'a str, start: &str) -> Option<&'a str> {
    let i = xml.find(start)?;
    let e = xml[i..].find('>')?;
    Some(&xml[i..=i + e])
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
    fn a_network_without_ipv4_is_not_ours() {
        assert!(
            import("<network><forward mode='bridge'/><bridge name='br0'/></network>").is_none()
        );
        assert!(
            import("<network><ip family='ipv6' address='fd00::1' prefix='64'/></network>")
                .is_none()
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
        assert_eq!(n.unique_name(&"z".repeat(63), "52:54:00:00:00:42"), "z".repeat(63));
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
        let dir = std::env::temp_dir().join(format!("ipam-test-{}", std::process::id()));
        let path = dir.join("ipam.json");
        let mut s = Store::default();
        s.networks.insert("internet".into(), net());
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), s);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left behind"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
