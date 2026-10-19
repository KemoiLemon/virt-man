//! Per-network firewall rules and NAT port forwards.
//!
//! libvirt has no network-level firewall primitive (nwfilters apply to domain
//! interfaces and have no "forward" direction or port forwarding), so these
//! rules are enforced directly on the host with nftables. The daemon renders
//! the whole set as one atomic `nft -f` script in its own `virt-man` table.
//! Rules persist in `/etc/virt-man-daemon/firewall.json` and are re-applied at
//! startup (nftables itself is not persistent across reboot).
//!
//! Direction semantics, mirroring a firewall appliance:
//!   - `in`      traffic entering the guest network (`ip daddr <subnet>`)
//!   - `out`     traffic leaving the guest network (`ip saddr <subnet>`)
//!   - `forward` all routed traffic (no subnet constraint)
//!
//! Port forwards are DNAT rules on the prerouting hook: a WAN destination
//! address (0.0.0.0 = any, excluding the guest subnet) and port are rewritten
//! to an internal address and port. The `vm-postrouting` chain masquerades a
//! NAT network's outbound traffic, so replies find their way back.
//!
//! The table also owns what libvirt's network driver used to: a per-network
//! accept/drop pair on the network's bridge (NAT forwards its guests, an
//! isolated network drops their outbound traffic), with the masquerade for the
//! NAT case. Those base rules are emitted after the explicit ones, so an
//! operator's own rule for the same traffic wins.
//!
//! The table is IPv4-only, like the daemon's IPAM. IPv6 is neither filtered nor
//! isolated here, and no such claim is made.

use crate::ipam::store::Mode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;

pub const FIREWALL_PATH: &str = "/etc/virt-man-daemon/firewall.json";

/// Everything persisted to disk: network rules/forwards and per-VM rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirewallStore {
    #[serde(default)]
    pub networks: HashMap<String, NetworkFirewall>,
    #[serde(default)]
    pub vms: HashMap<String, VmFirewall>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkFirewall {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub forwards: Vec<Forward>,
    #[serde(default)]
    pub policy: PolicySet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VmFirewall {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub policy: PolicySet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rule {
    pub direction: String, // in | out | forward
    pub action: String,    // accept | drop | reject
    pub proto: String,     // tcp | udp | icmp | any
    pub src_ip: String,    // CIDR or single IP, empty = any
    pub dst_ip: String,
    pub src_port: String, // "80" or "1000-2000", empty = any
    pub dst_port: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Forward {
    pub wan_ip: String, // "0.0.0.0" = any, otherwise a specific WAN address
    /// The destination port. A forward translates exactly one port: a range
    /// has no single corresponding internal port (DNAT maps a port to a port),
    /// so ranges were removed. An old stored range loads as its first port --
    /// `serde` accepts `wan_start` as the alias, and the end is dropped.
    #[serde(alias = "wan_start")]
    pub wan_port: u16,
    pub lan_ip: String,
    #[serde(alias = "lan_start")]
    pub lan_port: u16,
    pub proto: String, // tcp | udp
}

impl NetworkFirewall {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.forwards.is_empty() && self.policy.is_accept_all()
    }
}

impl VmFirewall {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.policy.is_accept_all()
    }
}

pub fn load(path: &Path) -> Result<FirewallStore, String> {
    if !path.exists() {
        return Ok(FirewallStore::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn save(path: &Path, store: &FirewallStore) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize firewall: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Validate a rule set, normalising CIDRs to `addr/prefix` form. Returns the
/// normalised rule, or an error string for the first invalid field.
pub fn validate_rule(r: &Rule) -> Result<Rule, String> {
    if !["in", "out", "forward"].contains(&r.direction.as_str()) {
        return Err("direction must be in, out or forward".into());
    }
    if !["accept", "drop", "reject"].contains(&r.action.as_str()) {
        return Err("action must be accept, drop or reject".into());
    }
    if !["tcp", "udp", "icmp", "any"].contains(&r.proto.as_str()) {
        return Err("protocol must be tcp, udp, icmp or any".into());
    }
    let src_ip = norm_cidr(&r.src_ip)?;
    let dst_ip = norm_cidr(&r.dst_ip)?;
    let src_port = norm_port(&r.src_port)?;
    let dst_port = norm_port(&r.dst_port)?;
    if (r.proto == "icmp" || r.proto == "any") && (!src_port.is_empty() || !dst_port.is_empty()) {
        return Err("ports apply only to tcp and udp rules".into());
    }
    Ok(Rule {
        direction: r.direction.clone(),
        action: r.action.clone(),
        proto: r.proto.clone(),
        src_ip,
        dst_ip,
        src_port,
        dst_port,
    })
}

pub fn validate_forward(f: &Forward) -> Result<Forward, String> {
    if !["tcp", "udp"].contains(&f.proto.as_str()) {
        return Err("forward protocol must be tcp or udp".into());
    }
    let lan_ip = parse_ip(&f.lan_ip)
        .map_err(|_| "internal IP is invalid")?
        .to_string();
    let wan_ip = if f.wan_ip == "0.0.0.0" {
        "0.0.0.0".to_string()
    } else {
        parse_ip(&f.wan_ip)
            .map_err(|_| "WAN IP is invalid")?
            .to_string()
    };
    let wan_port = valid_port(f.wan_port).map_err(|e| format!("WAN port: {e}"))?;
    let lan_port = valid_port(f.lan_port).map_err(|e| format!("internal port: {e}"))?;
    Ok(Forward {
        wan_ip,
        wan_port,
        lan_ip,
        lan_port,
        proto: f.proto.clone(),
    })
}

/// Normalise a CIDR or bare IP to `addr/prefix` (bare IP becomes /32); empty
/// passes through.
fn norm_cidr(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(String::new());
    }
    if let Ok(ip) = parse_ip(s) {
        return Ok(format!("{ip}/32"));
    }
    let (addr, bits) = s
        .split_once('/')
        .ok_or_else(|| format!("{s} is not an IP or CIDR"))?;
    let ip = parse_ip(addr).map_err(|_| format!("{addr} is not an IP"))?;
    let prefix: u8 = bits
        .parse()
        .map_err(|_| format!("{bits} is not a prefix length"))?;
    if prefix > 32 {
        return Err(format!("prefix {prefix} out of range"));
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let net = Ipv4Addr::from(u32::from(ip) & mask);
    Ok(format!("{net}/{prefix}"))
}

/// Normalise a port or "start-end" range; empty passes through.
fn norm_port(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(String::new());
    }
    let (a, b) = parse_port_range(s)?;
    if a == b {
        Ok(format!("{a}"))
    } else {
        Ok(format!("{a}-{b}"))
    }
}

fn parse_ip(s: &str) -> Result<Ipv4Addr, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{s} is not an IPv4 address"))
}

fn parse_port_range(s: &str) -> Result<(u16, u16), String> {
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (s.trim(), s.trim()),
    };
    let a: u16 = a.parse().map_err(|_| format!("{a} is not a port"))?;
    let b: u16 = b.parse().map_err(|_| format!("{b} is not a port"))?;
    if a == 0 || b == 0 {
        return Err("port 0 is not valid".into());
    }
    if a > b {
        return Err(format!("port range {a}-{b} is reversed"));
    }
    Ok((a, b))
}

fn valid_port(p: u16) -> Result<u16, String> {
    if p == 0 {
        return Err("port 0 is not valid".into());
    }
    Ok(p)
}

/// Default action for traffic not matched by any explicit rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Accept,
    Reject,
    Drop,
}

impl Policy {
    pub fn action(&self) -> &'static str {
        match self {
            Policy::Accept => "accept",
            Policy::Reject => "reject",
            Policy::Drop => "drop",
        }
    }
}

/// Per-direction default policies for a network or VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicySet {
    #[serde(default, rename = "in")]
    pub in_: Policy,
    #[serde(default, rename = "out")]
    pub out: Policy,
    #[serde(default, rename = "forward")]
    pub forward: Policy,
}

impl PolicySet {
    pub fn is_accept_all(&self) -> bool {
        self.in_ == Policy::Accept && self.out == Policy::Accept && self.forward == Policy::Accept
    }
}

// ------------------------------------------------------------------ render

/// One network's rules, forwards and default policies, with the subnet the
/// direction matches against. `subnet` is a network CIDR like 192.168.167.0/24.
pub struct RenderedNet<'a> {
    pub subnet: String,
    /// How the network reaches the world; decides its automatic base rules.
    pub mode: Mode,
    /// The bridge the guests are attached to.
    pub bridge: String,
    /// Whether the network is actually up: base rules exist only then, so a
    /// stopped network leaves nothing behind that could match another bridge.
    pub active: bool,
    pub rules: &'a [Rule],
    pub forwards: &'a [Forward],
    pub policy: PolicySet,
}

/// One VM's firewall rules and default policies, matched by the guest's
/// interface MAC addresses.
pub struct RenderedVm<'a> {
    pub macs: Vec<String>,
    pub rules: &'a [Rule],
    pub policy: PolicySet,
}

/// Build the complete nftables script for the `virt-man` table. Pure, so the
/// rule mapping is unit-testable without touching the host.
///
/// Explicit rules come first; per-direction default policies (anything not
/// ACCEPT) follow, scoped exactly like the rules they are the default for.
/// The chain base policy stays ACCEPT, so traffic outside every configured
/// scope falls through unharmed.
///
/// Every rule of a network that is not up is omitted, so a stopped network can
/// neither claim a WAN port nor match traffic that now belongs elsewhere.
pub fn render_ruleset(nets: &[RenderedNet], vms: &[RenderedVm]) -> String {
    let mut rules = Vec::new();
    let mut defaults = Vec::new();
    let mut forwards = Vec::new();
    let mut base = Vec::new();
    let mut masquerade = Vec::new();

    // Every bridge whose guests must never reach another network's guests.
    // A flat rule list means an accept for one network would otherwise end
    // evaluation before another network's drop is reached, so each accept for
    // our own traffic carries a guard against all of these.
    let guest_bridges: Vec<&str> = nets
        .iter()
        .filter(|n| n.active && n.mode != Mode::Bridge)
        .map(|n| n.bridge.as_str())
        .collect();

    for n in nets.iter().filter(|n| n.active) {
        for r in n.rules {
            if let Some(line) = render_rule(r, &n.subnet) {
                rules.push(line);
            }
        }
        defaults.extend(net_policy_defaults(n));
        for f in n.forwards {
            if let Some(line) = render_forward(f, &n.subnet) {
                forwards.push(line);
            }
        }
        // After the explicit rules and their defaults: those are the
        // operator's verdict for this traffic and must come first.
        base.extend(net_base_rules(n, &guest_bridges));
        if let Some(line) = net_masquerade(n) {
            masquerade.push(line);
        }
    }
    // A VM with no NIC cannot be matched by MAC, so skip it rather than emit
    // an unscoped host-wide rule.
    for v in vms.iter().filter(|v| !v.macs.is_empty()) {
        for r in v.rules {
            rules.extend(render_vm_rule(r, &v.macs));
        }
        defaults.extend(vm_policy_defaults(v));
    }

    let mut out = String::from("table ip virt-man {\n");
    out.push_str("  chain vm-forward {\n");
    out.push_str("    type filter hook forward priority filter - 1; policy accept;\n");
    for r in rules.iter().chain(defaults.iter()).chain(base.iter()) {
        out.push_str("    ");
        out.push_str(r);
        out.push('\n');
    }
    out.push_str("  }\n");
    out.push_str("  chain vm-dnat {\n");
    out.push_str("    type nat hook prerouting priority dstnat - 1; policy accept;\n");
    for f in forwards {
        out.push_str("    ");
        out.push_str(&f);
        out.push('\n');
    }
    out.push_str("  }\n");
    out.push_str("  chain vm-postrouting {\n");
    out.push_str("    type nat hook postrouting priority srcnat; policy accept;\n");
    for m in masquerade {
        out.push_str("    ");
        out.push_str(&m);
        out.push('\n');
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// An nft match that excludes every managed guest bridge: `oifname != "vbr1"`
/// for one, or a set for several. None when there is nothing to exclude.
fn not_guest_bridge(field: &str, bridges: &[&str]) -> Option<String> {
    match bridges {
        [] => None,
        [one] => Some(format!("{field} != \"{one}\"")),
        many => {
            let set: Vec<String> = many.iter().map(|b| format!("\"{b}\"")).collect();
            Some(format!("{field} != {{ {} }}", set.join(", ")))
        }
    }
}

/// The automatic per-network forwarding rules for a network that is up.
///
/// This is the half libvirt's network driver used to install: a NAT network
/// lets its guests out and accepts only replies and DNAT'd connections back
/// in; an isolated network drops guest traffic in both directions; a bridge
/// network is the operator's, and the operator's firewall owns its policy.
///
/// The accepts are guarded against every other managed bridge, because one
/// flat chain cannot "return to the network's own rules": an unguarded
/// `iifname <nat> accept` would admit a packet on its way to an isolated
/// network's guest, which the isolated network's `oifname <iso> drop` would
/// then never see.
fn net_base_rules(n: &RenderedNet, guest_bridges: &[&str]) -> Vec<String> {
    if !n.active {
        return Vec::new();
    }
    let b = &n.bridge;
    let out_guard = not_guest_bridge("oifname", guest_bridges);
    let in_guard = not_guest_bridge("iifname", guest_bridges);
    let guard = |g: &Option<String>| g.as_deref().map(|g| format!("{g} ")).unwrap_or_default();
    match n.mode {
        Mode::Nat => vec![
            format!("iifname \"{b}\" {}accept", guard(&out_guard)),
            format!(
                "oifname \"{b}\" {}ct state established,related accept",
                guard(&in_guard)
            ),
            format!("oifname \"{b}\" {}ct status dnat accept", guard(&in_guard)),
            format!("oifname \"{b}\" drop"),
        ],
        // `drop`, not `reject`: a guest sees a timeout as it would on a dead
        // link, and the host sends no ICMP it would have to route back. The
        // DNAT accept sits between the drops so an explicit port forward
        // still reaches its guest.
        Mode::Isolated => vec![
            format!("iifname \"{b}\" drop"),
            format!("oifname \"{b}\" {}ct status dnat accept", guard(&in_guard)),
            format!("oifname \"{b}\" drop"),
        ],
        Mode::Bridge => Vec::new(),
    }
}

/// The masquerade line for a NAT network that is up: guest traffic leaving by
/// any other interface is source-translated to the host's address. Traffic
/// that is already going to a guest (`ip daddr`) is excluded, so two guest
/// networks on one host do not get hairpinned through the host's address, and
/// multicast in 224.0.0.0/24 is left alone.
fn net_masquerade(n: &RenderedNet) -> Option<String> {
    (n.mode == Mode::Nat && n.active).then(|| {
        format!(
            "ip saddr {} ip daddr != {} ip daddr != 224.0.0.0/24 oifname != \"{}\" masquerade",
            n.subnet, n.subnet, n.bridge
        )
    })
}

/// Default-policy lines for a network. Only non-ACCEPT policies need a rule:
/// the chain base policy is ACCEPT, so anything without one falls through.
fn net_policy_defaults(n: &RenderedNet) -> Vec<String> {
    let mut out = Vec::new();
    if n.policy.in_ != Policy::Accept {
        out.push(format!("ip daddr {} {}", n.subnet, n.policy.in_.action()));
    }
    if n.policy.out != Policy::Accept {
        out.push(format!("ip saddr {} {}", n.subnet, n.policy.out.action()));
    }
    // forward is unscoped, matching how a forward rule behaves.
    if n.policy.forward != Policy::Accept {
        out.push(n.policy.forward.action().to_string());
    }
    out
}

/// Default-policy lines for a VM, scoped to its MACs. "forward" covers both
/// directions, so it emits two lines.
fn vm_policy_defaults(v: &RenderedVm) -> Vec<String> {
    let set = format!("{{ {} }}", v.macs.join(", "));
    let mut out = Vec::new();
    if v.policy.in_ != Policy::Accept {
        out.push(format!("ether daddr {set} {}", v.policy.in_.action()));
    }
    if v.policy.out != Policy::Accept {
        out.push(format!("ether saddr {set} {}", v.policy.out.action()));
    }
    if v.policy.forward != Policy::Accept {
        out.push(format!("ether saddr {set} {}", v.policy.forward.action()));
        out.push(format!("ether daddr {set} {}", v.policy.forward.action()));
    }
    out
}

/// The IP/port/protocol matches common to every rule, independent of whether
/// it is scoped to a network subnet or a VM's MAC.
fn rule_matches(r: &Rule) -> Vec<String> {
    let mut m = Vec::new();
    if !r.src_ip.is_empty() {
        m.push(format!("ip saddr {}", r.src_ip));
    }
    if !r.dst_ip.is_empty() {
        m.push(format!("ip daddr {}", r.dst_ip));
    }
    match r.proto.as_str() {
        "tcp" | "udp" => {
            let l4 = r.proto.as_str();
            let mut ports = Vec::new();
            if !r.src_port.is_empty() {
                ports.push(format!("{l4} sport {}", r.src_port));
            }
            if !r.dst_port.is_empty() {
                ports.push(format!("{l4} dport {}", r.dst_port));
            }
            if ports.is_empty() {
                m.push(format!("meta l4proto {l4}"));
            } else {
                m.extend(ports);
            }
        }
        "icmp" => m.push("meta l4proto icmp".to_string()),
        _ => {}
    }
    m
}

fn action_of(r: &Rule) -> &str {
    match r.action.as_str() {
        "drop" => "drop",
        "reject" => "reject",
        _ => "accept",
    }
}

fn render_rule(r: &Rule, subnet: &str) -> Option<String> {
    let mut m = match r.direction.as_str() {
        "in" => vec![format!("ip daddr {subnet}")],
        "out" => vec![format!("ip saddr {subnet}")],
        _ => Vec::new(),
    };
    m.extend(rule_matches(r));
    Some(format!("{} {}", m.join(" "), action_of(r)))
}

/// Render one VM rule. "forward" matches the guest's MAC in either direction,
/// so it emits two rules (source and destination) rather than a single
/// unscoped one.
fn render_vm_rule(r: &Rule, macs: &[String]) -> Vec<String> {
    let set = format!("{{ {} }}", macs.join(", "));
    let scopes: Vec<String> = match r.direction.as_str() {
        "in" => vec![format!("ether daddr {set}")],
        "out" => vec![format!("ether saddr {set}")],
        _ => vec![format!("ether saddr {set}"), format!("ether daddr {set}")],
    };
    let matches = rule_matches(r);
    scopes
        .into_iter()
        .map(|scope| {
            let mut m = vec![scope];
            m.extend(matches.iter().cloned());
            format!("{} {}", m.join(" "), action_of(r))
        })
        .collect()
}

fn render_forward(f: &Forward, subnet: &str) -> Option<String> {
    let mut m = Vec::new();
    if f.wan_ip == "0.0.0.0" {
        // "Any WAN address" must not DNAT traffic between guests, so exclude
        // the guest subnet as a destination.
        m.push(format!("ip daddr != {subnet}"));
    } else {
        m.push(format!("ip daddr {}", f.wan_ip));
    }
    let l4 = if f.proto == "udp" { "udp" } else { "tcp" };
    m.push(format!("{l4} dport {}", f.wan_port));
    Some(format!(
        "{} dnat to {}:{}",
        m.join(" "),
        f.lan_ip,
        f.lan_port
    ))
}

/// Apply a ruleset with `nft -f` (atomic: a syntax error aborts the whole
/// script and leaves the previous table in force). One transaction claims and
/// flushes the table, then fills it, so there is never a moment with no rules.
pub async fn apply_script(rendered: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let script = apply_script_input(rendered);
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn nft: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("nft stdin unavailable")?
        .write_all(script.as_bytes())
        .await
        .map_err(|e| format!("write nft script: {e}"))?;
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("nft wait: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The whole `nft -f` input: claim the table, empty it, then the rendered
/// block. `add table` succeeds whether or not the table exists, and the flush
/// removes every chain and rule before the block re-adds them — in one
/// transaction, so a bad script cannot leave the host with no firewall at all.
pub fn apply_script_input(rendered: &str) -> String {
    format!("add table ip virt-man\nflush table ip virt-man\n{rendered}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Rule {
        Rule {
            direction: "in".into(),
            action: "accept".into(),
            proto: "any".into(),
            src_ip: String::new(),
            dst_ip: String::new(),
            src_port: String::new(),
            dst_port: String::new(),
        }
    }

    /// A network with no explicit rules, for the automatic base rules.
    fn rendered<'a>(subnet: &str, mode: Mode, bridge: &str, active: bool) -> RenderedNet<'a> {
        RenderedNet {
            subnet: subnet.into(),
            mode,
            bridge: bridge.into(),
            active,
            rules: &[],
            forwards: &[],
            policy: PolicySet::default(),
        }
    }

    #[test]
    fn a_bare_ip_becomes_a_host_cidr() {
        assert_eq!(norm_cidr("192.168.1.5").unwrap(), "192.168.1.5/32");
        assert_eq!(norm_cidr("").unwrap(), "");
    }

    #[test]
    fn a_cidr_is_normalised_to_its_network_address() {
        assert_eq!(norm_cidr("192.168.167.1/24").unwrap(), "192.168.167.0/24");
        assert_eq!(norm_cidr("10.9.9.9/16").unwrap(), "10.9.0.0/16");
    }

    #[test]
    fn ports_accept_singles_and_ranges() {
        assert_eq!(norm_port("80").unwrap(), "80");
        assert_eq!(norm_port("1000-2000").unwrap(), "1000-2000");
        assert!(norm_port("2000-1000").is_err());
        assert!(norm_port("0").is_err());
    }

    #[test]
    fn rules_render_direction_and_matches() {
        let r = Rule {
            direction: "out".into(),
            proto: "tcp".into(),
            dst_port: "443".into(),
            ..base()
        };
        let out = render_rule(&r, "192.168.167.0/24").unwrap();
        assert_eq!(out, "ip saddr 192.168.167.0/24 tcp dport 443 accept");

        let r2 = Rule {
            direction: "forward".into(),
            action: "drop".into(),
            proto: "udp".into(),
            src_ip: "10.0.0.0/8".into(),
            ..base()
        };
        assert_eq!(
            render_rule(&r2, "192.168.167.0/24").unwrap(),
            "ip saddr 10.0.0.0/8 meta l4proto udp drop"
        );
    }

    #[test]
    fn a_specific_wan_forward_matches_that_address_only() {
        let f = Forward {
            wan_ip: "203.0.113.7".into(),
            wan_port: 2222,
            lan_ip: "192.168.167.5".into(),
            lan_port: 22,
            proto: "tcp".into(),
        };
        assert_eq!(
            render_forward(&f, "192.168.167.0/24").unwrap(),
            "ip daddr 203.0.113.7 tcp dport 2222 dnat to 192.168.167.5:22"
        );
    }

    #[test]
    fn an_any_wan_forward_excludes_the_guest_subnet() {
        let f = Forward {
            wan_ip: "0.0.0.0".into(),
            wan_port: 80,
            lan_ip: "192.168.167.5".into(),
            lan_port: 8080,
            proto: "tcp".into(),
        };
        assert_eq!(
            render_forward(&f, "192.168.167.0/24").unwrap(),
            "ip daddr != 192.168.167.0/24 tcp dport 80 dnat to 192.168.167.5:8080"
        );
    }

    #[test]
    fn the_ruleset_has_the_filter_dnat_and_postrouting_hooks() {
        let in_rule = Rule {
            direction: "in".into(),
            ..base()
        };
        let nets = [RenderedNet {
            subnet: "192.168.167.0/24".into(),
            mode: Mode::Nat,
            bridge: "virbr1".into(),
            active: true,
            rules: &[in_rule],
            forwards: &[],
            policy: PolicySet::default(),
        }];
        let out = render_ruleset(&nets, &[]);
        assert!(out.contains("hook forward priority filter - 1"));
        assert!(out.contains("hook prerouting priority dstnat - 1"));
        assert!(out.contains("hook postrouting priority srcnat"));
        assert!(out.contains("ip daddr 192.168.167.0/24 accept"));
    }

    #[test]
    fn an_active_nat_network_forwards_and_masquerades() {
        let nets = [rendered("10.9.0.0/24", Mode::Nat, "vbr5b92a1a5", true)];
        assert_eq!(
            render_ruleset(&nets, &[]),
            concat!(
                "table ip virt-man {\n",
                "  chain vm-forward {\n",
                "    type filter hook forward priority filter - 1; policy accept;\n",
                "    iifname \"vbr5b92a1a5\" oifname != \"vbr5b92a1a5\" accept\n",
                "    oifname \"vbr5b92a1a5\" iifname != \"vbr5b92a1a5\" ct state established,related accept\n",
                "    oifname \"vbr5b92a1a5\" iifname != \"vbr5b92a1a5\" ct status dnat accept\n",
                "    oifname \"vbr5b92a1a5\" drop\n",
                "  }\n",
                "  chain vm-dnat {\n",
                "    type nat hook prerouting priority dstnat - 1; policy accept;\n",
                "  }\n",
                "  chain vm-postrouting {\n",
                "    type nat hook postrouting priority srcnat; policy accept;\n",
                "    ip saddr 10.9.0.0/24 ip daddr != 10.9.0.0/24 ip daddr != 224.0.0.0/24 oifname != \"vbr5b92a1a5\" masquerade\n",
                "  }\n",
                "}\n",
            )
        );
    }

    #[test]
    fn an_active_isolated_network_drops_guest_traffic_and_masquerades_nothing() {
        // Port forwards still reach the guest: the DNAT accept sits ahead of
        // the outbound drop. Nothing is source-translated — an isolated
        // network has nowhere to forward to.
        let nets = [rendered("10.9.0.0/24", Mode::Isolated, "vbr5b92a1a5", true)];
        let out = render_ruleset(&nets, &[]);
        assert_eq!(
            out,
            concat!(
                "table ip virt-man {\n",
                "  chain vm-forward {\n",
                "    type filter hook forward priority filter - 1; policy accept;\n",
                "    iifname \"vbr5b92a1a5\" drop\n",
                "    oifname \"vbr5b92a1a5\" iifname != \"vbr5b92a1a5\" ct status dnat accept\n",
                "    oifname \"vbr5b92a1a5\" drop\n",
                "  }\n",
                "  chain vm-dnat {\n",
                "    type nat hook prerouting priority dstnat - 1; policy accept;\n",
                "  }\n",
                "  chain vm-postrouting {\n",
                "    type nat hook postrouting priority srcnat; policy accept;\n",
                "  }\n",
                "}\n",
            )
        );
        assert!(!out.contains("masquerade"), "{out}");
    }

    #[test]
    fn two_networks_guard_their_accepts_against_each_other() {
        // The NAT network is rendered first, which is the ordering that let a
        // flat `iifname accept` carry a packet into the isolated network
        // before its `oifname drop` was ever reached.
        let nets = [
            rendered("10.9.0.0/24", Mode::Nat, "vbr1", true),
            rendered("10.10.0.0/24", Mode::Isolated, "vbr2", true),
        ];
        let out = render_ruleset(&nets, &[]);
        for line in [
            "    iifname \"vbr1\" oifname != { \"vbr1\", \"vbr2\" } accept\n",
            "    oifname \"vbr1\" iifname != { \"vbr1\", \"vbr2\" } ct state established,related accept\n",
            "    oifname \"vbr1\" iifname != { \"vbr1\", \"vbr2\" } ct status dnat accept\n",
            "    oifname \"vbr1\" drop\n",
            "    iifname \"vbr2\" drop\n",
            "    oifname \"vbr2\" iifname != { \"vbr1\", \"vbr2\" } ct status dnat accept\n",
            "    oifname \"vbr2\" drop\n",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
        // The isolated network's drops are present and last, so whatever the
        // NAT accepts let through is still stopped there.
        let nat_out = out
            .find("iifname \"vbr1\" oifname != { \"vbr1\", \"vbr2\" } accept")
            .unwrap();
        let isolated_in = out.find("iifname \"vbr2\" drop").unwrap();
        let isolated_out = out.find("oifname \"vbr2\" drop").unwrap();
        assert!(nat_out < isolated_in && isolated_in < isolated_out);
    }

    #[test]
    fn an_inactive_network_emits_nothing_at_all() {
        // A stopped network must not keep its DNAT ports or any rule that
        // could match traffic another network now carries.
        let rule = Rule {
            direction: "forward".into(),
            proto: "tcp".into(),
            dst_port: "8443".into(),
            ..base()
        };
        let forward = Forward {
            wan_ip: "0.0.0.0".into(),
            wan_port: 8080,
            lan_ip: "10.9.0.10".into(),
            lan_port: 80,
            proto: "tcp".into(),
        };
        let policy = PolicySet {
            in_: Policy::Drop,
            out: Policy::Accept,
            forward: Policy::Accept,
        };
        let nets = [RenderedNet {
            subnet: "10.9.0.0/24".into(),
            mode: Mode::Nat,
            bridge: "vbr5b92a1a5".into(),
            active: false,
            rules: &[rule],
            forwards: &[forward],
            policy,
        }];
        let out = render_ruleset(&nets, &[]);
        assert_eq!(
            out,
            concat!(
                "table ip virt-man {\n",
                "  chain vm-forward {\n",
                "    type filter hook forward priority filter - 1; policy accept;\n",
                "  }\n",
                "  chain vm-dnat {\n",
                "    type nat hook prerouting priority dstnat - 1; policy accept;\n",
                "  }\n",
                "  chain vm-postrouting {\n",
                "    type nat hook postrouting priority srcnat; policy accept;\n",
                "  }\n",
                "}\n",
            )
        );
        for stale in ["8443", "8080", "iifname", "oifname", "drop", "masquerade"] {
            assert!(
                !out.contains(stale),
                "{stale} from a stopped network: {out}"
            );
        }
    }

    #[test]
    fn the_nft_input_claims_and_flushes_before_the_table() {
        // One transaction: `add` is idempotent, `flush` clears the old rules,
        // and the rendered table refills it. The table is declared exactly
        // once, so a failed script leaves the previous rules in force instead
        // of an empty table (the old delete-then-load did not).
        let rendered = render_ruleset(&[], &[]);
        let input = apply_script_input(&rendered);
        assert_eq!(input.matches("add table ip virt-man").count(), 1);
        assert_eq!(input.matches("flush table ip virt-man").count(), 1);
        assert_eq!(input.matches("table ip virt-man {").count(), 1);
        assert!(
            input.starts_with(
                "add table ip virt-man\nflush table ip virt-man\ntable ip virt-man {\n"
            ),
            "{input}"
        );
    }

    #[test]
    fn a_bridge_network_leaves_forwarding_to_the_operator() {
        // Bridge mode borrows a bridge the operator owns, so the operator's
        // firewall is the one that decides what crosses it.
        let nets = [rendered("10.9.0.0/24", Mode::Bridge, "br0", true)];
        let out = render_ruleset(&nets, &[]);
        assert!(!out.contains("br0"), "{out}");
        assert!(!out.contains("masquerade"), "{out}");
    }

    #[test]
    fn explicit_rules_precede_the_network_base_rules() {
        let explicit = Rule {
            direction: "forward".into(),
            proto: "tcp".into(),
            dst_port: "443".into(),
            ..base()
        };
        let nets = [RenderedNet {
            subnet: "10.9.0.0/24".into(),
            mode: Mode::Nat,
            bridge: "vbr5b92a1a5".into(),
            active: true,
            rules: &[explicit],
            forwards: &[],
            policy: PolicySet {
                in_: Policy::Drop,
                ..PolicySet::default()
            },
        }];
        let out = render_ruleset(&nets, &[]);
        let explicit_pos = out.find("tcp dport 443 accept").unwrap();
        let default_pos = out.find("ip daddr 10.9.0.0/24 drop").unwrap();
        let base_pos = out.find("iifname \"vbr5b92a1a5\" oifname").unwrap();
        assert!(
            explicit_pos < default_pos,
            "explicit rules precede defaults"
        );
        assert!(
            default_pos < base_pos,
            "the automatic base rules come last: {out}"
        );
    }

    #[test]
    fn a_vm_rule_matches_the_guest_mac_by_direction() {
        let macs = vec!["52:54:00:aa:bb:cc".to_string()];
        let in_rule = Rule {
            direction: "in".into(),
            proto: "tcp".into(),
            dst_port: "443".into(),
            ..base()
        };
        let lines = render_vm_rule(&in_rule, &macs);
        assert_eq!(
            lines,
            vec!["ether daddr { 52:54:00:aa:bb:cc } tcp dport 443 accept".to_string()]
        );

        // forward emits both directions.
        let fwd = Rule {
            direction: "forward".into(),
            ..base()
        };
        let lines = render_vm_rule(&fwd, &macs);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("ether saddr"));
        assert!(lines[1].starts_with("ether daddr"));
    }

    #[test]
    fn non_accept_policies_emit_scoped_defaults_after_explicit_rules() {
        let explicit = Rule {
            direction: "in".into(),
            proto: "tcp".into(),
            dst_port: "443".into(),
            ..base()
        };
        let policy = PolicySet {
            in_: Policy::Drop,
            out: Policy::Accept,
            forward: Policy::Reject,
        };
        let nets = [RenderedNet {
            subnet: "192.168.167.0/24".into(),
            // Bridge mode adds no base rules, so this test sees only the
            // policy defaults it is about.
            mode: Mode::Bridge,
            bridge: "virbr1".into(),
            active: true,
            rules: &[explicit],
            forwards: &[],
            policy,
        }];
        let out = render_ruleset(&nets, &[]);
        let explicit_pos = out.find("tcp dport 443 accept").unwrap();
        let in_default = out.find("ip daddr 192.168.167.0/24 drop").unwrap();
        let fwd_default = out.find("reject").unwrap();
        assert!(explicit_pos < in_default, "explicit rules precede defaults");
        assert!(
            in_default < fwd_default,
            "in default precedes the unscoped forward default"
        );
        assert!(
            !out.contains("ip saddr 192.168.167.0/24"),
            "ACCEPT defaults emit no rule"
        );
    }

    #[test]
    fn a_vm_with_no_macs_contributes_no_rules() {
        let vm = RenderedVm {
            macs: vec![],
            rules: &[Rule {
                direction: "in".into(),
                ..base()
            }],
            policy: PolicySet::default(),
        };
        let out = render_ruleset(&[], &[vm]);
        // The chain header says "policy accept"; no rule line may contain an
        // ether match (a NIC-less VM cannot be scoped).
        assert!(
            !out.contains("ether "),
            "no unscoped rules for a NIC-less VM"
        );
    }
}
