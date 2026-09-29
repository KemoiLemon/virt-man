//! Per-network firewall rules and NAT port forwards.
//!
//! libvirt has no network-level firewall primitive (nwfilters apply to domain
//! interfaces and have no "forward" direction or port forwarding), so these
//! rules are enforced directly on the host with nftables. The daemon renders
//! the whole set as one atomic `nft -f` script in its own `virt-man` table,
//! which is separate from libvirt's `libvirt_network` table so the two never
//! fight. Rules persist in `/etc/virt-man/firewall.json` and are re-applied at
//! startup (nftables itself is not persistent across reboot).
//!
//! Direction semantics, mirroring a firewall appliance:
//!   - `in`      traffic entering the guest network (`ip daddr <subnet>`)
//!   - `out`     traffic leaving the guest network (`ip saddr <subnet>`)
//!   - `forward` all routed traffic (no subnet constraint)
//!
//! Port forwards are DNAT rules on the prerouting hook: a WAN destination
//! address (0.0.0.0 = any, excluding the guest subnet) and port range are
//! rewritten to an internal address and port range. libvirt's existing
//! postrouting masquerade handles the reply path.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;

pub const FIREWALL_PATH: &str = "/etc/virt-man/firewall.json";

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
    pub wan_start: u16,
    pub wan_end: u16,
    pub lan_ip: String,
    pub lan_start: u16,
    pub lan_end: u16,
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
    let (ws, we) = port_range(f.wan_start, f.wan_end).map_err(|e| format!("WAN port: {e}"))?;
    let (ls, le) = port_range(f.lan_start, f.lan_end).map_err(|e| format!("internal port: {e}"))?;
    Ok(Forward {
        wan_ip,
        wan_start: ws,
        wan_end: we,
        lan_ip,
        lan_start: ls,
        lan_end: le,
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

fn port_range(a: u16, b: u16) -> Result<(u16, u16), String> {
    if a == 0 || b == 0 {
        return Err("port 0 is not valid".into());
    }
    if a > b {
        return Err(format!("port range {a}-{b} is reversed"));
    }
    Ok((a, b))
}

/// The network address of a CIDR (192.168.167.1/24 -> 192.168.167.0/24).
pub fn network_prefix(cidr: &str) -> Option<String> {
    let (addr, bits) = cidr.split_once('/')?;
    let ip: Ipv4Addr = addr.parse().ok()?;
    let prefix: u8 = bits.parse().ok()?;
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some(format!("{}/{prefix}", Ipv4Addr::from(u32::from(ip) & mask)))
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
pub fn render_ruleset(nets: &[RenderedNet], vms: &[RenderedVm]) -> String {
    let mut rules = Vec::new();
    let mut defaults = Vec::new();
    let mut forwards = Vec::new();
    for n in nets {
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
    for r in rules.iter().chain(defaults.iter()) {
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
    out.push_str("}\n");
    out
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
    let dport = if f.wan_start == f.wan_end {
        format!("{l4} dport {}", f.wan_start)
    } else {
        format!("{l4} dport {}-{}", f.wan_start, f.wan_end)
    };
    m.push(dport);
    let target = if f.lan_start == f.lan_end {
        format!("{}:{}", f.lan_ip, f.lan_start)
    } else {
        format!("{}:{}-{}", f.lan_ip, f.lan_start, f.lan_end)
    };
    Some(format!("{} dnat to {target}", m.join(" ")))
}

/// Apply a ruleset with `nft -f` (atomic: a syntax error aborts the whole
/// script). A prior `virt-man` table is flushed first so removing a rule or a
/// whole network does not leave stale entries behind.
pub async fn apply_script(script: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    // Ignore failure: the table may not exist yet.
    let _ = Command::new("nft")
        .args(["delete", "table", "ip", "virt-man"])
        .output()
        .await;

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

/// libvirt's chain for traffic entering a network, on its **iptables**
/// backend. It accepts replies to guest-initiated flows and then rejects
/// everything else, per network.
const LIBVIRT_NET_IN: &str = "LIBVIRT_FWI";

/// Where libvirt's **nftables** backend keeps the same filter: a table of its
/// own, with the entering-traffic rules in `guest_input`.
const LIBVIRT_NFT_TABLE: &str = "libvirt_network";
const LIBVIRT_NFT_CHAIN: &str = "guest_input";

/// Marks the rules we insert, so re-asserting cannot stack copies and a later
/// reader can tell ours from libvirt's own.
///
/// The surrounding quotes are part of the value, not decoration: nft takes a
/// comment as a single quoted token, so passing it bare splits it into extra
/// arguments and nft fails with "syntax error, unexpected dnat" — after which
/// forwarding is still broken and nothing looks wrong from our side. Both
/// consumers want the quotes included (nft's grammar, and the listed output
/// where the marker is recognised), so they live here rather than at the call
/// site.
const OUR_MARK: &str = "\"virt-man dnat\"";

/// [`OUR_MARK`] without its surrounding quotes, for matching what `nft list`
/// prints back.
fn our_mark_quoted() -> String {
    OUR_MARK.trim_matches('"').to_string()
}

/// What one libvirt firewall backend had to say:
/// - `Ok(true)`  — this backend is in use and forwards are now admitted;
/// - `Ok(false)` — this backend is not in use here, so nothing to do;
/// - `Err`       — the backend is in use and could not be satisfied.
type Backend = Result<bool, String>;

/// The iptables arguments that admit a network's DNAT'd traffic through
/// libvirt's filter. Pure, so the rule's shape is unit-testable.
///
/// `--ctstate DNAT` is the whole point: it matches only connections that were
/// destination-translated, i.e. exactly the port forwards, and leaves
/// libvirt's filtering of everything else untouched.
pub fn forward_accept_rule(subnet: &str) -> Vec<String> {
    [
        "-d",
        subnet,
        "-m",
        "conntrack",
        "--ctstate",
        "DNAT",
        "-j",
        "ACCEPT",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The nftables arguments for the same job. Pure, so it is unit-testable.
///
/// The matcher is spelled `ct status dnat` here, not `ct state dnat`:
/// nftables keeps the translation flags in the conntrack *status* field, and
/// rejects `ct state dnat` outright ("Could not parse conntrack state"), where
/// iptables accepts it as a pseudo-state. Same meaning, different spelling, so
/// it is worth pinning: this one fails loudly when wrong, but a reader
/// "unifying" the two spellings would break it.
pub fn forward_accept_nft(subnet: &str) -> Vec<String> {
    [
        "insert",
        "rule",
        "ip",
        LIBVIRT_NFT_TABLE,
        LIBVIRT_NFT_CHAIN,
        "ip",
        "daddr",
        subnet,
        "ct",
        "status",
        "dnat",
        "counter",
        "accept",
        "comment",
        OUR_MARK,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Whether a rule of ours that the backend already has can admit anything at
/// all: it must sit **ahead of** libvirt's reject in the rule order.
///
/// Both backends list a chain in rule order, and that order is the whole
/// question — libvirt *inserts* its accept+reject at the head of its chain
/// whenever it (re)applies a network, i.e. on a network start and on a
/// libvirtd restart. That pushes our rule below the reject, and `iptables -C`
/// or a `contains` on the nft listing only asks whether the rule exists, so
/// they keep answering "already there" while every forwarded connection is
/// rejected: a forward's first packet is NEW, so the reject throws it away
/// before our accept is ever consulted.
///
/// So a rule that is present but behind the reject has to be *moved*, not left
/// alone — that is the difference between "the fix is applied" and "the fix is
/// in force".
fn ahead_of_reject(listing: &str, ours: impl Fn(&str) -> bool) -> bool {
    for line in listing.lines() {
        if ours(line) {
            return true;
        }
        // libvirt's own rejection, spelled `-j REJECT` or `reject with icmp`.
        if line.contains("reject") || line.contains("-j DROP") {
            return false;
        }
    }
    false
}

/// The nft rule handles on the lines matching `mark`, so a stray or mis-placed
/// copy can be removed before ours is inserted again.
fn nft_handles(listing: &str, mark: &str) -> Vec<u64> {
    listing
        .lines()
        .filter(|l| l.contains(mark))
        .filter_map(|l| l.rsplit("# handle ").next()?.trim().parse().ok())
        .collect()
}

/// Admit a network's DNAT'd traffic into libvirt's **iptables** filter.
async fn allow_via_iptables(subnet: &str) -> Backend {
    use tokio::process::Command;
    let rule = forward_accept_rule(subnet);

    // -S lists the chain in order, in one call, which answers both questions:
    // whether our rule exists and whether it is ahead of the reject. Asking
    // with -C instead only answered the first, which is how a rule that
    // libvirt had pushed behind its reject kept looking applied.
    let Ok(listing) = Command::new("iptables")
        .args(["-S", LIBVIRT_NET_IN])
        .output()
        .await
    else {
        // No iptables at all, so libvirt's chains cannot exist either.
        return Ok(false);
    };
    if !listing.status.success() {
        // No such chain: this host's libvirt does not filter through
        // iptables — most likely it uses the nftables backend, which the
        // other path handles.
        return Ok(false);
    }
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    let prefix = format!("-A {LIBVIRT_NET_IN} ");
    let ours =
        |l: &str| l.starts_with(&prefix) && l.contains("--ctstate DNAT") && l.contains(subnet);
    if ahead_of_reject(&listing, ours) {
        // In force, which is the common case on every pass after the first.
        return Ok(true);
    }

    // Missing, or present but behind the reject: drop every copy, then put one
    // at the head. Deleting first is what keeps this idempotent, so
    // re-asserting cannot stack copies.
    for _ in 0..listing.lines().filter(|l| ours(l)).count() {
        let _ = Command::new("iptables")
            .arg("-D")
            .arg(LIBVIRT_NET_IN)
            .args(&rule)
            .output()
            .await;
    }

    let out = Command::new("iptables")
        .arg("-I")
        .arg(LIBVIRT_NET_IN)
        .arg("1")
        .args(&rule)
        .output()
        .await
        .map_err(|e| format!("iptables: {e}"))?;
    if out.status.success() {
        tracing::info!(
            "firewall: admitting DNAT'd traffic to {subnet} ahead of libvirt's reject (iptables)"
        );
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("No chain") {
        return Ok(false);
    }
    Err(format!("iptables -I {LIBVIRT_NET_IN}: {}", err.trim()))
}

/// Admit a network's DNAT'd traffic into libvirt's **nftables** filter.
async fn allow_via_nftables(subnet: &str) -> Backend {
    use tokio::process::Command;

    // Reading the chain serves twice over: it says whether this host uses the
    // nftables backend at all, and whether our rule is already in place.
    let Ok(current) = Command::new("nft")
        .args(["list", "chain", "ip", LIBVIRT_NFT_TABLE, LIBVIRT_NFT_CHAIN])
        .output()
        .await
    else {
        return Ok(false);
    };
    if !current.status.success() {
        return Ok(false);
    }
    let listing = String::from_utf8_lossy(&current.stdout).into_owned();
    let mark = our_mark_quoted();
    if ahead_of_reject(&listing, |l| l.contains(&mark)) {
        return Ok(true);
    }

    // Behind the reject, or missing: drop our copies by handle first, so
    // re-inserting cannot stack them.
    for handle in nft_handles(&listing, &mark) {
        let _ = Command::new("nft")
            .args([
                "delete",
                "rule",
                "ip",
                LIBVIRT_NFT_TABLE,
                LIBVIRT_NFT_CHAIN,
                "handle",
                &handle.to_string(),
            ])
            .output()
            .await;
    }

    // Plain `insert` prepends, putting ours ahead of libvirt's reject.
    let out = Command::new("nft")
        .args(forward_accept_nft(subnet))
        .output()
        .await
        .map_err(|e| format!("nft: {e}"))?;
    if out.status.success() {
        tracing::info!(
            "firewall: admitting DNAT'd traffic to {subnet} ahead of libvirt's reject (nftables)"
        );
        return Ok(true);
    }
    Err(format!(
        "nft insert into {LIBVIRT_NFT_TABLE}/{LIBVIRT_NFT_CHAIN}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// Let a network's port forwards past libvirt's own firewall.
///
/// libvirt renders, per network, an accept for `related,established` traffic
/// into the bridge followed by a reject of everything else. On its iptables
/// backend that is `LIBVIRT_FWI`:
///
/// ```text
/// -A LIBVIRT_FWI -d 10.74.0.0/24 -o virbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
/// -A LIBVIRT_FWI -o virbr0 -j REJECT --reject-with icmp-port-unreachable
/// ```
///
/// and on its nftables backend the same two rules live in
/// `ip libvirt_network guest_input`. A forwarded connection's first packet is
/// NEW, not established, so libvirt throws it away — *after* our own DNAT has
/// rewritten it and our own rules have accepted it. The guest never sees the
/// connection and the client is told "connection refused", because the reject
/// answers with ICMP port-unreachable. Measured on real hosts, on both
/// backends: libvirt's reject counter rises by exactly one per connection
/// attempt, while the DNAT itself is correct.
///
/// Nothing in our own table can prevent that. A verdict in one chain does not
/// stop a later chain of the same hook from rejecting, so accepting at
/// `priority filter - 1` — which is what this table already does for its own
/// rules — does not save the packet. The accept has to sit inside libvirt's
/// chain, ahead of its reject. libvirt has no way to declare a port forward
/// itself (`network.rng` has no such element), so there is nowhere else for it
/// to go.
///
/// Both backends are attempted and whichever is in use is satisfied — a host
/// uses one or the other, so the other reports that it is not present.
/// Idempotent, and safe to call on a timer: libvirt rewrites those chains when
/// a network is (re)started or libvirtd restarts, which silently undoes this,
/// so the caller re-asserts it rather than applying it once.
pub async fn allow_forwarded(subnet: &str) -> Result<(), String> {
    let iptables = allow_via_iptables(subnet).await;
    let nftables = allow_via_nftables(subnet).await;
    match (iptables, nftables) {
        // Neither backend in use: nothing to get past.
        (Ok(false), Ok(false)) => Ok(()),
        // One of them is in use and now admits the traffic.
        (Ok(true), _) | (_, Ok(true)) => Ok(()),
        // In use and unsatisfied: say so rather than pretend it worked.
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
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
    fn the_forward_accept_admits_only_translated_traffic() {
        // These rules go inside libvirt's own chains, ahead of the reject that
        // would otherwise drop a port forward's first packet. They must not
        // widen libvirt's filtering beyond the connections we translated:
        // scoped to the network, and to DNAT'd flows only.
        let r = forward_accept_rule("10.74.0.0/24");
        assert_eq!(
            r,
            vec![
                "-d",
                "10.74.0.0/24",
                "-m",
                "conntrack",
                "--ctstate",
                "DNAT",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(LIBVIRT_NET_IN, "LIBVIRT_FWI", "libvirt's inbound chain");
        assert!(
            !r.iter().any(|a| a.contains("RELATED")),
            "must not restate libvirt's established-traffic accept"
        );
        assert!(!r.iter().any(|a| a == "DROP" || a == "REJECT"));
    }

    #[test]
    fn a_rule_behind_libvirts_reject_is_not_admitting_anything() {
        // The real shape, measured on an iptables-backed host whose forward
        // worked before a network restart and not after: libvirt inserted its
        // own two rules at the head, ours drifted to the end, and the reject
        // then counted one packet per connection attempt.
        let ours = |l: &str| l.contains("--ctstate DNAT");
        let in_force = "\
-P LIBVIRT_FWI ACCEPT
-A LIBVIRT_FWI -d 10.74.0.0/24 -m conntrack --ctstate DNAT -j ACCEPT
-A LIBVIRT_FWI -d 10.74.0.0/24 -o virbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A LIBVIRT_FWI -o virbr0 -j REJECT --reject-with icmp-port-unreachable";
        assert!(ahead_of_reject(in_force, ours), "ahead of the reject");

        let drifted = "\
-A LIBVIRT_FWI -o virbr0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A LIBVIRT_FWI -o virbr0 -j REJECT --reject-with icmp-port-unreachable
-A LIBVIRT_FWI -d 10.74.0.0/24 -m conntrack --ctstate DNAT -j ACCEPT";
        assert!(
            !ahead_of_reject(drifted, ours),
            "present, but behind the reject, so it admits nothing"
        );
        assert!(!ahead_of_reject("-P LIBVIRT_FWI ACCEPT", ours), "absent");
    }

    #[test]
    fn the_nft_chain_is_judged_the_same_way_and_our_copies_are_findable() {
        let mark = our_mark_quoted();
        let ours = |l: &str| l.contains(&mark);
        let listing = "\
chain guest_input {
		type filter hook forward priority filter; policy accept;
		oif \"virbr0\" ip daddr 10.74.0.0/24 ct state established,related accept
		oif \"virbr0\" reject with icmp type port-unreachable
		ip daddr 10.74.0.0/24 ct status dnat counter accept comment \"virt-man dnat\" # handle 9
}";
        assert!(!ahead_of_reject(listing, ours), "behind libvirt's reject");
        assert_eq!(nft_handles(listing, &mark), vec![9], "deletable by handle");
        let moved = listing.replace(
            "ip daddr 10.74.0.0/24 ct status dnat counter accept comment \"virt-man dnat\" # handle 9",
            "",
        );
        assert!(
            !ahead_of_reject(&moved, ours),
            "removing ours leaves none, so it is re-inserted"
        );
    }

    #[test]
    fn the_forward_accept_is_spelled_for_each_backend() {
        // The two backends name the same concept differently. nftables keeps
        // translation in the conntrack *status* field and refuses
        // `ct state dnat`; iptables takes DNAT as a pseudo-state. Pinning both
        // spellings stops a later "unification" that would parse-error on a
        // host using the other backend.
        let nft = forward_accept_nft("10.74.0.0/24");
        let joined = nft.join(" ");
        assert!(
            joined.contains("ct status dnat"),
            "nftables' spelling: {joined}"
        );
        assert!(
            !joined.contains("ct state dnat"),
            "which nft refuses to parse"
        );
        assert!(
            joined.starts_with("insert rule"),
            "prepended, so it beats the reject"
        );
        assert!(
            joined.contains("ip daddr 10.74.0.0/24"),
            "scoped to the network"
        );
        assert!(
            joined.contains(OUR_MARK),
            "marked, so re-asserting is idempotent"
        );
        assert_eq!(LIBVIRT_NFT_TABLE, "libvirt_network");
        assert_eq!(LIBVIRT_NFT_CHAIN, "guest_input");
        // The comment must already carry its quotes. Bare, nft splits it into
        // extra arguments and refuses the whole rule with "syntax error,
        // unexpected dnat" — measured on a real host, where that left
        // forwarding broken while the rest of the rule read correctly.
        assert_eq!(
            nft.last().map(String::as_str),
            Some("\"virt-man dnat\""),
            "the comment reaches nft as one quoted token"
        );
        assert_eq!(
            our_mark_quoted(),
            "virt-man dnat",
            "matching is on the bare text nft lists back"
        );

        let iptables = forward_accept_rule("10.74.0.0/24").join(" ");
        assert!(
            iptables.contains("--ctstate DNAT"),
            "iptables' spelling: {iptables}"
        );
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
            wan_start: 2222,
            wan_end: 2222,
            lan_ip: "192.168.167.5".into(),
            lan_start: 22,
            lan_end: 22,
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
            wan_start: 80,
            wan_end: 80,
            lan_ip: "192.168.167.5".into(),
            lan_start: 8080,
            lan_end: 8080,
            proto: "tcp".into(),
        };
        assert_eq!(
            render_forward(&f, "192.168.167.0/24").unwrap(),
            "ip daddr != 192.168.167.0/24 tcp dport 80 dnat to 192.168.167.5:8080"
        );
    }

    #[test]
    fn the_ruleset_has_both_chains_with_the_right_hooks() {
        let in_rule = Rule {
            direction: "in".into(),
            ..base()
        };
        let nets = [RenderedNet {
            subnet: "192.168.167.0/24".into(),
            rules: &[in_rule],
            forwards: &[],
            policy: PolicySet::default(),
        }];
        let out = render_ruleset(&nets, &[]);
        assert!(out.contains("hook forward priority filter - 1"));
        assert!(out.contains("hook prerouting priority dstnat - 1"));
        assert!(out.contains("ip daddr 192.168.167.0/24 accept"));
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
