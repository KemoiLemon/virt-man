//! The host side of a network the daemon owns.
//!
//! The daemon creates the bridge, gives it the gateway address, runs dnsmasq
//! on it for DHCP and DNS, and enforces NAT and forwarding itself. Nothing
//! else may define the same bridge — two owners of one bridge fight over its
//! address and its dnsmasq.
//!
//! `Mode::Bridge` is the exception: it borrows a bridge the operator made, so
//! here we only check it is really a bridge, never create or delete it.
//!
//! The network API drives the lifecycle ([`HostNet::start`]/[`HostNet::stop`])
//! and the supervisor in `ipam::server` keeps dnsmasq matched to the store.
//! The firewall reads [`bridge_name`], [`bridge_up`] and [`is_active`] from
//! here, and enables forwarding through [`enable_ip_forward`].

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::ipam::store::{Mode, Net, Reservation, normalize_mac, valid_domain};

/// Where pid and config files live. Tmpfs, so a reboot starts clean.
pub const RUN_DIR: &str = "/run/virt-man-daemon";
pub(crate) const SYS_NET: &str = "/sys/class/net";
const DNSMASQ: &str = "/usr/sbin/dnsmasq";

/// A deterministic bridge name for a network id: `vbr` plus twelve hex digits,
/// 15 characters — the Linux interface-name limit. Same id, same bridge,
/// across restarts.
///
/// An id that parses as a UUID uses its first 12 hex digits (the spelling with
/// or without dashes gives the same name). 48 bits rather than 32: two
/// caller-chosen UUIDs sharing a prefix must not land on one bridge, or one
/// network would flush and then delete the other's. The first 12 hex can still
/// coincide, so the network API refuses a create or edit whose derived name
/// another stored network already holds. Anything else is hashed with FNV-1a
/// 64, low 48 bits kept, so a non-UUID id gets a distinct name too.
pub fn bridge_name(id: &str) -> String {
    if let Ok(u) = uuid::Uuid::parse_str(id) {
        return format!("vbr{}", &u.simple().to_string()[..12]);
    }
    format!("vbr{:012x}", fnv1a(id) & 0xffff_ffff_ffff)
}

/// FNV-1a, 64-bit: small, stable across builds, and we need no dependency.
/// Shared with the QEMU manager, whose tap and MAC names must also be stable.
pub(crate) fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The bridge this network uses: ours for a managed network, the operator's
/// for a bridge network. A managed network's imported bridge (`virbrN`) is
/// deliberately ignored — that bridge is not ours.
pub fn bridge_for(net: &Net, id: &str) -> String {
    if net.mode == Mode::Bridge {
        net.bridge.clone()
    } else {
        bridge_name(id)
    }
}

/// A Linux interface name we are willing to hand to `ip`: at most 15
/// characters, only characters a kernel name uses, and no leading `-` so it
/// cannot be read as an option. Also the rule a bridge network's name must
/// pass before it is stored.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Any interface by that name exists.
pub fn bridge_exists(name: &str) -> bool {
    valid_name(name) && Path::new(SYS_NET).join(name).exists()
}

/// The interface exists and the kernel says it is a bridge.
pub fn is_bridge(name: &str) -> bool {
    valid_name(name) && Path::new(SYS_NET).join(name).join("bridge").exists()
}

/// Whether a network's bridge is up. This, not process health, is what the
/// firewall follows: rules must stay while dnsmasq is restarting.
pub fn bridge_up(net: &Net, id: &str) -> bool {
    bridge_exists(&bridge_for(net, id))
}

/// Whether a network is up on the host: its bridge exists and, for a network
/// that serves addresses, its dnsmasq is running. Read-only, so a caller
/// polling this can never start or stop anything.
pub fn is_active(net: &Net, id: &str) -> bool {
    bridge_up(net, id) && (!net.is_served() || dnsmasq_running_in(Path::new(RUN_DIR), id))
}

/// A tun/tap (a guest's tap) that is free, or already on `bridge`. Anything
/// else — a physical NIC, a veth, a tap another bridge already owns — is never
/// enslaved: moving it would take a device from whatever is really using it.
fn attachable(port: &str, bridge: &str) -> bool {
    if !valid_name(port) || !Path::new(SYS_NET).join(port).join("tun_flags").exists() {
        return false;
    }
    let master = Path::new(SYS_NET).join(port).join("master");
    match std::fs::read_link(&master) {
        Ok(target) => target.file_name().is_some_and(|n| n == bridge),
        // No `master` link: not enslaved, so free to attach.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        // Could not tell who owns it: leave it alone rather than guess.
        Err(_) => false,
    }
}

/// The interfaces plugged into a bridge, stable-sorted.
pub fn bridge_ports(name: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(Path::new(SYS_NET).join(name).join("brif")) else {
        return Vec::new();
    };
    let mut ports: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    ports.sort();
    ports
}

/// The `ip` commands that bring a network's bridge up, in order, given what
/// the host currently has. Split from `ensure_plan` so the decisions can be
/// tested without root or a real sysfs.
fn plan_for(
    bridge: &str,
    mode: Mode,
    cidr: Option<&str>,
    exists: bool,
    is_a_bridge: bool,
    current: &[String],
) -> Result<Vec<Vec<String>>, String> {
    // Never take over an interface of another kind that happens to hold the
    // name: `ip link add` would fail, and addressing it would be worse.
    if exists && !is_a_bridge {
        return Err(format!(
            "'{bridge}' already exists and is not a Linux bridge; refusing to use it"
        ));
    }
    if mode == Mode::Bridge {
        if !exists {
            return Err(format!("bridge '{bridge}' does not exist"));
        }
        return Ok(Vec::new());
    }
    let mut plan = Vec::new();
    if !exists {
        plan.push(vec![
            "link".into(),
            "add".into(),
            "name".into(),
            bridge.into(),
            "type".into(),
            "bridge".into(),
        ]);
    }
    if let Some(cidr) = cidr {
        // Only reprogram when the address set must change. Flushing and
        // re-adding the same gateway drops the subnet for a moment, and with
        // it the DNS and DHCP server its guests are using.
        if current.len() != 1 || current[0] != cidr {
            // A stale gateway must not stay live beside the new one: it would
            // answer ARP/DNS for a subnet this network no longer owns.
            plan.push(vec![
                "-4".into(),
                "addr".into(),
                "flush".into(),
                "dev".into(),
                bridge.into(),
            ]);
            plan.push(vec![
                "addr".into(),
                "replace".into(),
                cidr.into(),
                "dev".into(),
                bridge.into(),
            ]);
        }
    }
    plan.push(vec![
        "link".into(),
        "set".into(),
        bridge.into(),
        "up".into(),
    ]);
    Ok(plan)
}

fn ensure_plan(net: &Net, id: &str) -> Result<Vec<Vec<String>>, String> {
    let bridge = bridge_for(net, id);
    if !valid_name(&bridge) {
        return Err(format!("network '{id}' has no usable bridge name"));
    }
    let exists = bridge_exists(&bridge);
    let current = if exists {
        ipv4_addrs(&bridge)
    } else {
        Vec::new()
    };
    plan_for(
        &bridge,
        net.mode,
        net.cidr().as_deref(),
        exists,
        exists && is_bridge(&bridge),
        &current,
    )
}

/// The IPv4 addresses on an interface, as `a.b.c.d/prefix`, in `ip`'s order.
/// Used to tell "already programmed" from "must be reprogrammed"; an interface
/// that does not exist answers none, which is the fresh-bridge case.
fn ipv4_addrs(bridge: &str) -> Vec<String> {
    let Ok(out) = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", bridge])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_addrs(&String::from_utf8_lossy(&out.stdout))
}

/// `3: vbr0    inet 10.9.0.1/24 brd 10.9.0.255 scope global vbr0` -> the
/// `inet` value, per line. Pure, so the `ip` output shape is testable.
fn parse_addrs(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields.position(|t| t == "inet")?;
            fields.next().map(str::to_string)
        })
        .collect()
}

fn failure(prog: &str, args: &[String], stderr: &[u8]) -> String {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg.trim();
    if msg.is_empty() {
        format!("{prog} {}", args.join(" "))
    } else {
        format!("{prog} {}: {msg}", args.join(" "))
    }
}

async fn run(prog: &str, args: &[String]) -> Result<(), String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("run {prog}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failure(prog, args, &out.stderr))
    }
}

/// Bring a network's bridge up (or verify the operator's).
pub async fn ensure_bridge_async(net: &Net, id: &str) -> Result<(), String> {
    for args in ensure_plan(net, id)? {
        run("ip", &args).await?;
    }
    Ok(())
}

/// Delete a managed network's bridge. An operator's bridge is never touched,
/// and an already-absent bridge is success.
pub async fn delete_bridge(net: &Net, id: &str) -> Result<(), String> {
    if !net.is_managed() {
        return Ok(());
    }
    let bridge = bridge_for(net, id);
    // Only something the kernel agrees is a bridge: a name that now belongs to
    // another kind of interface is not ours to delete.
    if !is_bridge(&bridge) {
        return Ok(());
    }
    let args = vec!["link".into(), "delete".into(), bridge];
    match run("ip", &args).await {
        // Lost a race with something else removing it: that is the outcome.
        Err(e) if e.contains("Cannot find device") => Ok(()),
        other => other,
    }
}

/// Plug ports back into a bridge after it was recreated. Only a free tun/tap
/// is attached; a port whose guest is gone, and anything that is not a tap or
/// already belongs to another bridge, is skipped.
pub async fn reattach_ports(net: &Net, id: &str, ports: &[String]) -> Result<(), String> {
    let bridge = bridge_for(net, id);
    for port in ports {
        if !attachable(port, &bridge) {
            tracing::warn!("not attaching {port} to {bridge}: not a free tun/tap");
            continue;
        }
        let args = vec![
            "link".into(),
            "set".into(),
            port.clone(),
            "master".into(),
            bridge.clone(),
        ];
        if let Err(e) = run("ip", &args).await {
            if e.contains("Cannot find device") {
                tracing::warn!("{port} vanished before it could be attached");
                continue;
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Enable IPv4 forwarding on the host. This belongs to ruleset application,
/// after the NAT rules exist — a bare bridge is not a reason to forward
/// everything. It only ever turns forwarding on: the sysctl is host-wide and
/// shared with Docker and every other service on the host, so writing 0 here
/// would break them silently. Nothing clears it.
pub fn enable_ip_forward() -> Result<(), String> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        .map_err(|e| format!("enable ip_forward: {e}"))
}

// ------------------------------------------------------------ dnsmasq

/// Rotate past this, keeping one old file. A query log at `extra` verbosity
/// grows without bound otherwise, and nothing else prunes it.
const LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Whether a log of this size is rotated. Separate so the rule is testable
/// without a real dnsmasq or a 64 MiB file.
fn should_rotate(size: u64) -> bool {
    size > LOG_MAX_BYTES
}

/// The `dnsmasq-<id>` / `net-<id>` file name part, with anything that could
/// escape the directory removed.
fn safe_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// One line of dnsmasq's lease file: when the lease expires, which MAC holds
/// it, and which address it is. dnsmasq also writes a hostname and client id
/// on the end, which this daemon does not need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Unix seconds. 0 means the lease never expires.
    pub expires: u64,
    pub mac: String,
    pub ip: Ipv4Addr,
}

/// `<expiry> <mac> <ip> <hostname|*> <client-id|*>` per line. A malformed
/// line is skipped, not fatal: dnsmasq may be mid-write when it is read.
pub fn parse_leases(text: &str) -> Vec<Lease> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let expires = fields.next()?.parse().ok()?;
            let mac = normalize_mac(fields.next()?)?;
            let ip = fields.next()?.parse().ok()?;
            Some(Lease { expires, mac, ip })
        })
        .collect()
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Alive, not a zombie (a zombie's cmdline reads empty), and never pid 1.
fn proc_alive(pid: u32) -> bool {
    pid > 1 && !cmdline(pid).is_empty()
}

fn cmdline_has(pid: u32, needle: &str) -> bool {
    let line = cmdline(pid);
    !line.is_empty() && line.contains(needle)
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether dnsmasq is running for `id` with `run_dir`'s config. Free, so
/// [`is_active`] can ask without a `HostNet`: the pid file is all it needs.
fn dnsmasq_running_in(run_dir: &Path, id: &str) -> bool {
    let file = safe_id(id);
    let Some(pid) = read_pid(&run_dir.join(format!("dnsmasq-{file}.pid"))) else {
        return false;
    };
    cmdline_has(
        pid,
        &run_dir
            .join(format!("dnsmasq-{file}.conf"))
            .to_string_lossy(),
    )
}

fn signal(pid: u32, sig: libc::c_int) -> Result<(), String> {
    // SAFETY: kill(2) with a pid already checked to be our dnsmasq.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("signal {pid}: {err}"))
}

/// Write a file only the daemon can read: dnsmasq's config names forwarders
/// and reservations, and its pid file picks the process to stop.
fn write_0600(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    // An old file keeps its previous mode through `open`; set it explicitly.
    let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    f.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[derive(Debug, Clone)]
pub struct HostNet {
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl HostNet {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            run_dir: PathBuf::from(RUN_DIR),
        }
    }

    fn ports_path(&self, id: &str) -> PathBuf {
        self.run_dir.join(format!("net-{}.ports", safe_id(id)))
    }
    /// Where this network's dnsmasq config lives. The supervisor compares it
    /// with what the stored state now renders, so a changed reservation is
    /// picked up even when nobody restarted dnsmasq by hand.
    pub fn conf_path(&self, id: &str) -> PathBuf {
        self.run_dir.join(format!("dnsmasq-{}.conf", safe_id(id)))
    }
    fn pid_path(&self, id: &str) -> PathBuf {
        self.run_dir.join(format!("dnsmasq-{}.pid", safe_id(id)))
    }
    /// Where dnsmasq writes this network's query log. The supervisor reads it
    /// for the access log's DNS half, so it is part of the interface.
    pub fn log_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(format!("dnsmasq-{}.log", safe_id(id)))
    }

    /// Remember the taps that are on a bridge, so a restart can plug them back
    /// in. Names that are not usable interface names are dropped. An empty
    /// list writes nothing: a transient empty read must not discard taps.
    pub fn save_ports(&self, id: &str, ports: &[String]) -> Result<(), String> {
        let mut text = String::new();
        for port in ports.iter().filter(|p| valid_name(p)) {
            text.push_str(port);
            text.push('\n');
        }
        if text.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.run_dir)
            .map_err(|e| format!("create {}: {e}", self.run_dir.display()))?;
        write_0600(&self.ports_path(id), text.as_bytes())
    }

    /// The saved ports, leaving the file in place: a failed start must not
    /// lose them.
    pub fn read_ports(&self, id: &str) -> Vec<String> {
        std::fs::read_to_string(self.ports_path(id))
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|l| valid_name(l))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forget the saved ports, once they are back on the bridge (or the
    /// network is being deleted).
    pub fn clear_ports(&self, id: &str) {
        let _ = std::fs::remove_file(self.ports_path(id));
    }

    /// Delete every file this network left in the data and run directories.
    /// The caller stops dnsmasq first; it will be called on network delete.
    pub fn remove(&self, id: &str) {
        let file = safe_id(id);
        let mut paths = vec![
            self.conf_path(id),
            self.pid_path(id),
            self.ports_path(id),
            self.log_path(id),
            self.data_dir.join(format!("dnsmasq-{file}.log.1")),
            self.data_dir.join(format!("dnsmasq-{file}.leases")),
        ];
        paths.sort();
        paths.dedup();
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }

    fn leases_path(&self, id: &str) -> PathBuf {
        self.data_dir
            .join(format!("dnsmasq-{}.leases", safe_id(id)))
    }

    /// The leases dnsmasq currently holds for a network: when each address was
    /// last renewed, from which MAC. Empty when there is no lease file yet.
    pub fn read_leases(&self, id: &str) -> Vec<Lease> {
        std::fs::read_to_string(self.leases_path(id))
            .map(|text| parse_leases(&text))
            .unwrap_or_default()
    }

    /// The pid file's process, but only when it really is our dnsmasq for this
    /// network: alive, and its `cmdline` names this network's config. The pid
    /// file alone is bookkeeping and can name anything after a crash or a
    /// reused pid, so it is never signalled unchecked.
    fn verified_pid(&self, id: &str) -> Option<u32> {
        let pid = read_pid(&self.pid_path(id))?;
        if !proc_alive(pid) || !cmdline_has(pid, &self.conf_path(id).to_string_lossy()) {
            return None;
        }
        Some(pid)
    }

    /// Keep the query log bounded: past the cap, move it aside and start
    /// fresh, keeping a single old file. Called on start and from the
    /// supervisor's pass, so a long-lived dnsmasq is pruned too.
    pub fn rotate_log(&self, id: &str) {
        let log = self.log_path(id);
        let Ok(meta) = std::fs::metadata(&log) else {
            return;
        };
        if !should_rotate(meta.len()) {
            return;
        }
        let old = self.data_dir.join(format!("dnsmasq-{}.log.1", safe_id(id)));
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(&log, &old).is_ok() {
            let _ = write_0600(&log, b"");
            // A rename alone leaves dnsmasq writing to the old descriptor, so
            // the tail of `.log` would see no new lookups and `.log.1` would
            // keep growing. SIGUSR2 makes dnsmasq reopen its log file; only a
            // pid whose cmdline names our conf is signalled.
            if let Some(pid) = self.verified_pid(id)
                && let Err(e) = signal(pid, libc::SIGUSR2)
            {
                tracing::warn!("ipam: asking dnsmasq for {id} to reopen its log: {e}");
            }
        }
    }

    /// dnsmasq's configuration for one network. `reservations` are all of the
    /// network's named addresses (every one answers DNS); `dhcp_macs` are the
    /// guests this host actually runs, and only those get a `dhcp-host`: the
    /// DHCP range is static, so an address is handed out only to a MAC named
    /// here, which is what keeps strangers off the network.
    pub fn dnsmasq_conf(
        &self,
        net: &Net,
        id: &str,
        reservations: &[Reservation],
        dhcp_macs: &HashSet<String>,
    ) -> String {
        if !net.is_served() {
            return String::new();
        }
        let bridge = bridge_for(net, id);
        let mut c = format!(
            "interface={bridge}\nbind-interfaces\nlisten-address={}\nport=53\nno-hosts\n",
            net.gateway
        );
        let domain = net.domain.as_deref().and_then(valid_domain);
        // `local=` makes the domain authoritative: an unknown name under it
        // is NXDOMAIN, not a query leaked to whatever resolver is upstream.
        if let Some(d) = &domain {
            c.push_str(&format!("domain={d}\nlocal=/{d}/\n"));
        }
        // Static-only DHCP: dnsmasq 2.93 rejects `start,end,<lease>,static`
        // ("bad dhcp-range"). A single address followed by `static` is the
        // accepted form, and the range bounds mean nothing here anyway — only
        // the dhcp-host entries below are ever handed out, so the store's
        // range stays the allocator's business, not dnsmasq's. A static range
        // carries no lease duration either; dnsmasq's own default (one hour,
        // the store's default too) applies.
        c.push_str(&format!("dhcp-range={},static\n", net.range_start));
        for r in reservations.iter().filter(|r| dhcp_macs.contains(&r.mac)) {
            match r.name.as_deref().filter(|n| !n.is_empty()) {
                Some(name) => c.push_str(&format!("dhcp-host={},{},{name}\n", r.mac, r.ip)),
                None => c.push_str(&format!("dhcp-host={},{}\n", r.mac, r.ip)),
            }
        }
        for r in reservations {
            let Some(name) = r.name.as_deref().filter(|n| !n.is_empty()) else {
                continue;
            };
            c.push_str(&format!("host-record={name},{}\n", r.ip));
            if let Some(d) = &domain {
                c.push_str(&format!("host-record={name}.{d},{}\n", r.ip));
            }
        }
        for f in net.dns_forwarders.iter().filter(|f| **f != net.gateway) {
            c.push_str(&format!("server={f}\n"));
        }
        if !net.dns_forwarders.is_empty() {
            c.push_str("no-resolv\n");
        }
        let data = self.data_dir.display();
        let run = self.run_dir.display();
        let file_id = safe_id(id);
        c.push_str(&format!("dhcp-leasefile={data}/dnsmasq-{file_id}.leases\n"));
        c.push_str(&format!("pid-file={run}/dnsmasq-{file_id}.pid\n"));
        c.push_str("log-queries=extra\n");
        c.push_str(&format!("log-facility={data}/dnsmasq-{file_id}.log\n"));
        c
    }

    /// Start dnsmasq for a network and wait until it is demonstrably running.
    /// A network with nothing to serve starts nothing.
    pub async fn start_dnsmasq(
        &self,
        net: &Net,
        id: &str,
        reservations: &[Reservation],
        dhcp_macs: &HashSet<String>,
    ) -> Result<(), String> {
        let conf = self.dnsmasq_conf(net, id, reservations, dhcp_macs);
        if conf.is_empty() {
            return Ok(());
        }
        for dir in [&self.data_dir, &self.run_dir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let path = self.conf_path(id);
        write_0600(&path, conf.as_bytes())?;
        // A second start must not leave the old process bound to port 53 with
        // the previous reservations and domain.
        self.stop_dnsmasq(id).await?;
        // Nothing holds the log now, so rotating is a plain rename.
        self.rotate_log(id);
        let exe = if Path::new(DNSMASQ).exists() {
            DNSMASQ
        } else {
            "dnsmasq"
        };
        let child = Command::new(exe)
            .arg(format!("--conf-file={}", path.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("start {exe}: {e}"))?;
        let mut stderr = String::new();
        // dnsmasq daemonises, so this waits for the parent's exit, not the
        // server's. The pid file below is what proves it actually started.
        match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
            Ok(Ok(out)) if !out.status.success() => {
                stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("wait for {exe}: {e}")),
            // Did not exit in five seconds; the pid file still decides.
            Err(_) => {}
        }
        for _ in 0..20 {
            if self.dnsmasq_running(id) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(if stderr.is_empty() {
            format!("dnsmasq did not start for network '{id}'")
        } else {
            format!("dnsmasq refused {}: {stderr}", path.display())
        })
    }

    /// Stop, then start again, so a config change takes effect. dnsmasq reads
    /// its config once at startup: a rewritten file alone changes nothing.
    pub async fn restart_dnsmasq(
        &self,
        net: &Net,
        id: &str,
        reservations: &[Reservation],
        dhcp_macs: &HashSet<String>,
    ) -> Result<(), String> {
        self.stop_dnsmasq(id).await?;
        self.start_dnsmasq(net, id, reservations, dhcp_macs).await
    }

    /// Stop our dnsmasq for a network. A pid file naming anything else is
    /// stale bookkeeping and is removed without signalling that process.
    pub async fn stop_dnsmasq(&self, id: &str) -> Result<(), String> {
        if let Some(pid) = self.verified_pid(id) {
            signal(pid, libc::SIGTERM)?;
            for _ in 0..30 {
                if !proc_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if proc_alive(pid) {
                signal(pid, libc::SIGKILL)?;
            }
        }
        let _ = std::fs::remove_file(self.pid_path(id));
        Ok(())
    }

    pub fn dnsmasq_running(&self, id: &str) -> bool {
        self.verified_pid(id).is_some()
    }

    /// Bring a network up: its bridge, its guests' taps and its dnsmasq.
    /// `enable_ip_forward` is the ruleset's job, once NAT rules exist.
    pub async fn start(
        &self,
        net: &Net,
        id: &str,
        reservations: &[Reservation],
        dhcp_macs: &HashSet<String>,
    ) -> Result<(), String> {
        ensure_bridge_async(net, id).await?;
        if !net.is_managed() {
            return Ok(());
        }
        // Read without consuming: a failed attach must leave the list for the
        // next attempt.
        let ports = self.read_ports(id);
        reattach_ports(net, id, &ports).await?;
        self.clear_ports(id);
        if net.is_served() {
            self.start_dnsmasq(net, id, reservations, dhcp_macs).await?;
        }
        Ok(())
    }

    /// Bring a managed network down, keeping its taps in the run directory so
    /// a later start plugs them back in. An operator's bridge is left alone.
    pub async fn stop(&self, net: &Net, id: &str) -> Result<(), String> {
        if !net.is_managed() {
            return Ok(());
        }
        let ports = bridge_ports(&bridge_for(net, id));
        self.save_ports(id, &ports)?;
        self.stop_dnsmasq(id).await?;
        delete_bridge(net, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(mode: Mode) -> Net {
        Net {
            title: "lab".into(),
            mode,
            bridge: if mode == Mode::Bridge {
                "br0".into()
            } else {
                String::new()
            },
            autostart: true,
            gateway: "10.9.0.1".parse().unwrap(),
            prefix: 24,
            range_start: "10.9.0.50".parse().unwrap(),
            range_end: "10.9.0.99".parse().unwrap(),
            dns_forwarders: vec!["1.1.1.1".parse().unwrap()],
            domain: Some("lab".into()),
            lease_secs: 3600,
            reservations: Vec::new(),
        }
    }

    fn reservation(mac: &str, ip: &str, name: Option<&str>) -> Reservation {
        Reservation {
            mac: mac.into(),
            ip: ip.parse().unwrap(),
            name: name.map(str::to_string),
            dynamic: false,
            vm: None,
            seen_ms: 0,
        }
    }

    /// A HostNet whose files all land in one throwaway directory.
    fn host_in(dir: &std::path::Path) -> HostNet {
        HostNet {
            data_dir: dir.to_path_buf(),
            run_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn a_bridge_name_is_short_stable_and_unique_per_id() {
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let name = bridge_name(id);
        assert_eq!(name, "vbr5b92a1a54f00");
        assert_eq!(name.len(), 15, "the Linux interface-name limit");
        assert_eq!(name, bridge_name(id), "same id, same bridge");
        assert_eq!(
            name,
            bridge_name("5b92a1a54f004a1c9f2e1d3c5b7a9e00"),
            "the two spellings of one UUID are the same network"
        );
        assert!(name.len() <= 15);
        assert_ne!(name, bridge_name("5b92a1a6-4f00-4a1c-9f2e-1d3c5b7a9e00"));
        // 48 bits, not 32: two ids sharing their first 8 hex digits still
        // differ once their next four do, so one network cannot land on
        // another's bridge. (Identical first 12 would still collide; the
        // network API refuses that explicitly.)
        assert_ne!(
            bridge_name("5b92a1a5-0001-4000-8000-000000000001"),
            bridge_name("5b92a1a5-0002-4000-8000-000000000001")
        );

        // An id that is not a UUID is hashed: stable, 15 characters, and two
        // ids with no hex digits at all still get different names.
        for other in ["internet", "internal", "ab", "cd", "../etc/x"] {
            let n = bridge_name(other);
            assert_eq!(n, bridge_name(other), "{other} is not stable");
            assert!(n.starts_with("vbr") && n.len() == 15, "{n}");
        }
        assert_ne!(bridge_name("ab"), bridge_name("cd"));
        assert_ne!(bridge_name("ab"), "vbr000000000000");
        assert_ne!(bridge_name("internet"), bridge_name("internal"));
    }

    #[test]
    fn subnet_and_cidr_come_from_the_gateway_and_prefix() {
        let n = net(Mode::Nat);
        assert_eq!(n.subnet().as_deref(), Some("10.9.0.0/24"));
        assert_eq!(n.cidr().as_deref(), Some("10.9.0.1/24"));
        assert!(n.is_managed() && n.is_served());

        let mut none = net(Mode::Nat);
        none.gateway = "0.0.0.0".parse().unwrap();
        assert_eq!(none.subnet(), None);
        assert_eq!(none.cidr(), None);
        assert!(!none.is_served());

        // A bridge network has no address of ours to describe.
        let b = net(Mode::Bridge);
        assert_eq!(b.subnet(), None);
        assert_eq!(b.cidr(), None);
        assert!(!b.is_managed() && !b.is_served());
    }

    #[test]
    fn the_bridge_plan_verifies_creates_flushes_and_raises() {
        // An operator's bridge is only checked.
        assert!(
            plan_for("br0", Mode::Bridge, None, true, true, &[])
                .unwrap()
                .is_empty()
        );
        assert!(
            plan_for("br0", Mode::Bridge, None, false, false, &[]).is_err(),
            "a bridge network never creates its bridge"
        );

        // A managed network's bridge is created, then any stale address is
        // flushed before the gateway is programmed and the bridge raised.
        let plan = plan_for(
            "vbr0123abcd",
            Mode::Nat,
            Some("10.9.0.1/24"),
            false,
            false,
            &[],
        )
        .unwrap();
        let steps: Vec<String> = plan.iter().map(|a| a.join(" ")).collect();
        assert_eq!(
            steps,
            [
                "link add name vbr0123abcd type bridge",
                "-4 addr flush dev vbr0123abcd",
                "addr replace 10.9.0.1/24 dev vbr0123abcd",
                "link set vbr0123abcd up",
            ]
        );

        // An existing managed bridge is not re-added, but is still flushed
        // when its address set must change.
        let plan = plan_for(
            "vbr0123abcd",
            Mode::Isolated,
            Some("10.9.1.1/24"),
            true,
            true,
            &["10.9.0.1/24".to_string()],
        )
        .unwrap();
        let steps: Vec<String> = plan.iter().map(|a| a.join(" ")).collect();
        assert_eq!(steps[0], "-4 addr flush dev vbr0123abcd");
        assert!(
            !steps.iter().any(|s| s.starts_with("link add")),
            "{steps:?}"
        );

        // Already carrying exactly the wanted address: nothing is dropped, so
        // the gateway (and the DNS on it) never goes away for a Start.
        let plan = plan_for(
            "vbr0123abcd",
            Mode::Nat,
            Some("10.9.0.1/24"),
            true,
            true,
            &["10.9.0.1/24".to_string()],
        )
        .unwrap();
        let steps: Vec<String> = plan.iter().map(|a| a.join(" ")).collect();
        assert_eq!(steps, ["link set vbr0123abcd up"]);

        // A second, stale address still forces the flush.
        let plan = plan_for(
            "vbr0123abcd",
            Mode::Nat,
            Some("10.9.0.1/24"),
            true,
            true,
            &["10.9.0.1/24".to_string(), "10.9.9.1/24".to_string()],
        )
        .unwrap();
        assert!(
            plan.iter()
                .any(|a| a.join(" ") == "-4 addr flush dev vbr0123abcd"),
            "{plan:?}"
        );
    }

    #[test]
    fn ip_addr_output_is_read_as_inet_addresses() {
        let text = "3: vbr0123abcd    inet 10.9.0.1/24 brd 10.9.0.255 scope global vbr0123abcd\n\
                    4: vbrfeedface    inet 10.9.9.1/24 brd 10.9.9.255 scope global vbrfeedface\n";
        assert_eq!(
            parse_addrs(text),
            vec!["10.9.0.1/24".to_string(), "10.9.9.1/24".to_string()]
        );
        assert!(parse_addrs("").is_empty());
        assert!(parse_addrs("no addresses here").is_empty());
    }

    #[test]
    fn an_interface_that_is_not_a_bridge_is_never_adopted() {
        let err = plan_for("lo", Mode::Nat, Some("10.9.0.1/24"), true, false, &[]).unwrap_err();
        assert!(
            err.contains("lo") && err.contains("not a Linux bridge"),
            "{err}"
        );
        // Through the real path too: the loopback exists on every host and is
        // not a bridge.
        let mut n = net(Mode::Bridge);
        n.bridge = "lo".into();
        let err = ensure_plan(&n, "x").unwrap_err();
        assert!(err.contains("lo"), "{err}");
    }

    #[test]
    fn dnsmasq_gets_the_subnet_reservations_domain_and_forwarders() {
        let dir = std::env::temp_dir().join(format!("hostnet-conf-{}", std::process::id()));
        let host = host_in(&dir);
        let id = "n1";
        let reservations = [
            reservation("52:54:00:aa:bb:cc", "10.9.0.10", Some("web")),
            reservation("52:54:00:aa:bb:cd", "10.9.0.11", None),
            // Not a guest of this host: answers DNS, gets no DHCP address.
            reservation("52:54:00:aa:bb:ce", "10.9.0.12", Some("printer")),
        ];
        let macs: HashSet<String> = ["52:54:00:aa:bb:cc", "52:54:00:aa:bb:cd"]
            .iter()
            .map(|m| m.to_string())
            .collect();
        let conf = host.dnsmasq_conf(&net(Mode::Nat), id, &reservations, &macs);
        for line in [
            format!("interface={}", bridge_name(id)),
            "bind-interfaces".into(),
            "listen-address=10.9.0.1".into(),
            "port=53".into(),
            "no-hosts".into(),
            "domain=lab".into(),
            "local=/lab/".into(),
            "dhcp-range=10.9.0.50,static".into(),
            "dhcp-host=52:54:00:aa:bb:cc,10.9.0.10,web".into(),
            "dhcp-host=52:54:00:aa:bb:cd,10.9.0.11".into(),
            "host-record=web,10.9.0.10".into(),
            "host-record=web.lab,10.9.0.10".into(),
            "host-record=printer,10.9.0.12".into(),
            "server=1.1.1.1".into(),
            "no-resolv".into(),
            format!("dhcp-leasefile={}/dnsmasq-n1.leases", dir.display()),
            format!("pid-file={}/dnsmasq-n1.pid", dir.display()),
            "log-queries=extra".into(),
            format!("log-facility={}/dnsmasq-n1.log", dir.display()),
        ] {
            assert!(
                conf.lines().any(|l| l == line),
                "missing {line:?} in:\n{conf}"
            );
        }
        assert!(
            conf.contains("dhcp-host=52:54:00:aa:bb:cd,10.9.0.11\n"),
            "an unnamed reservation has no third field"
        );
        assert!(
            !conf.contains("dhcp-host=52:54:00:aa:bb:ce"),
            "a MAC no guest of this host has never gets an address:\n{conf}"
        );
    }

    #[test]
    fn a_hostile_domain_cannot_inject_dnsmasq_options() {
        let host = HostNet::new(std::env::temp_dir());
        let mut n = net(Mode::Nat);
        n.domain = Some("lab\nlisten-address=0.0.0.0".into());
        let conf = host.dnsmasq_conf(&n, "n3", &[], &HashSet::new());
        assert!(!conf.contains("0.0.0.0"), "{conf}");
        assert!(
            !conf.lines().any(|l| l.starts_with("domain=")),
            "an invalid domain is omitted, not repaired:\n{conf}"
        );
        // A valid one is normalised before it reaches the file.
        let mut n = net(Mode::Nat);
        n.domain = Some("  LAB.Example ".into());
        let conf = host.dnsmasq_conf(&n, "n4", &[], &HashSet::new());
        assert!(conf.contains("domain=lab.example\n"), "{conf}");
        assert!(conf.contains("local=/lab.example/\n"), "{conf}");
    }

    #[test]
    fn dnsmasq_is_never_pointed_at_its_own_gateway() {
        let host = HostNet::new(std::env::temp_dir());
        let mut n = net(Mode::Nat);
        n.dns_forwarders = vec!["10.9.0.1".parse().unwrap(), "1.1.1.1".parse().unwrap()];
        let conf = host.dnsmasq_conf(&n, "n5", &[], &HashSet::new());
        assert!(!conf.contains("server=10.9.0.1\n"), "{conf}");
        assert!(conf.contains("server=1.1.1.1\n"), "{conf}");
    }

    #[test]
    fn a_bridge_network_or_a_gatewayless_one_needs_no_dnsmasq() {
        let host = HostNet::new(std::env::temp_dir());
        assert!(
            host.dnsmasq_conf(&net(Mode::Bridge), "b1", &[], &HashSet::new())
                .is_empty()
        );
        let mut none = net(Mode::Nat);
        none.gateway = "0.0.0.0".parse().unwrap();
        assert!(
            host.dnsmasq_conf(&none, "n2", &[], &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn a_network_without_its_bridge_is_not_active() {
        // Read-only: nothing here creates an interface, so both answers are
        // false on any host.
        let mut operator = net(Mode::Bridge);
        operator.bridge = "vmtest-nobr".into();
        assert!(!bridge_exists(&operator.bridge));
        assert!(!is_active(&operator, "b1"));

        let id = "00000000-0000-0000-0000-000000000000";
        assert!(!bridge_exists(&bridge_name(id)));
        assert!(!is_active(&net(Mode::Nat), id));
    }

    #[test]
    fn saved_ports_survive_a_read_and_only_valid_names_are_kept() {
        let dir = std::env::temp_dir().join(format!("hostnet-ports-{}", std::process::id()));
        let host = host_in(&dir);
        let ports = ["vnet0".to_string(), "vnet1".to_string()];
        host.save_ports("abc-123", &ports).unwrap();
        assert_eq!(host.read_ports("abc-123"), ports.to_vec());
        assert_eq!(
            host.read_ports("abc-123"),
            ports.to_vec(),
            "reading does not consume the list"
        );
        host.clear_ports("abc-123");
        assert!(host.read_ports("abc-123").is_empty());

        // A transient empty read must not discard a saved list.
        host.save_ports("abc-123", &ports).unwrap();
        host.save_ports("abc-123", &[]).unwrap();
        assert_eq!(host.read_ports("abc-123"), ports.to_vec());

        // A name that is not an interface name never reaches a command.
        host.save_ports(
            "abc-123",
            &[
                "vnet0".into(),
                "bad name".into(),
                "-x".into(),
                "vnet:1".into(),
            ],
        )
        .unwrap();
        assert_eq!(host.read_ports("abc-123"), vec!["vnet0".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_hostile_network_id_cannot_escape_the_run_dir() {
        let dir = std::env::temp_dir().join(format!("hostnet-safe-{}", std::process::id()));
        let host = host_in(&dir);
        assert_eq!(safe_id("../../etc/x"), "etcx");
        assert!(!safe_id("../..").contains('/'));
        let path = host.ports_path("../../etc/x");
        assert_eq!(path.parent(), Some(dir.as_path()));
        host.save_ports("../../etc/x", &["vnet0".into()]).unwrap();
        assert!(dir.join("net-etcx.ports").exists());
        assert!(!dir.join("..").join("etc").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The line dnsmasq actually accepts. `dhcp-range=start,end,lease,static`
    /// was rejected outright ("bad dhcp-range"), so this runs the real binary
    /// over the whole generated file: every directive is checked, not just the
    /// range.
    #[test]
    fn the_generated_config_passes_dnsmasq_syntax_check() {
        let exe = if Path::new(DNSMASQ).exists() {
            DNSMASQ
        } else {
            "dnsmasq"
        };
        let Ok(version) = std::process::Command::new(exe).arg("--version").output() else {
            eprintln!("skipping: dnsmasq is not installed");
            return;
        };
        if !version.status.success() {
            eprintln!("skipping: `{exe} --version` failed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hostnet-syntax-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = host_in(&dir);
        let reservations = [
            reservation("52:54:00:aa:bb:cc", "10.9.0.10", Some("web")),
            reservation("52:54:00:aa:bb:cd", "10.9.0.11", None),
            reservation("52:54:00:aa:bb:ce", "10.9.0.12", Some("printer")),
        ];
        let macs: HashSet<String> = ["52:54:00:aa:bb:cc", "52:54:00:aa:bb:cd"]
            .iter()
            .map(|m| m.to_string())
            .collect();
        let conf = host.dnsmasq_conf(&net(Mode::Nat), "n1", &reservations, &macs);
        let path = dir.join("check.conf");
        std::fs::write(&path, &conf).unwrap();
        let out = std::process::Command::new(exe)
            .args(["--test", "-C"])
            .arg(&path)
            .output()
            .expect("run dnsmasq --test");
        assert!(
            out.status.success(),
            "dnsmasq rejected the generated config:\n{}{}\n{conf}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lease_lines_are_read_as_mac_address_and_expiry() {
        // dnsmasq writes a hostname and client id on the end; a `*` means
        // "none". A line dnsmasq is halfway through writing is skipped.
        let text = "1790000000 52:54:00:AA:BB:CC 10.9.0.10 web 01:52:54:00:aa:bb:cc\n\
                    1790003600 52-54-00-aa-bb-cd 10.9.0.11 * *\n\
                    garbage\n\
                    1790007200 52:54:00:aa:bb:cf not-an-ip x *\n";
        assert_eq!(
            parse_leases(text),
            vec![
                Lease {
                    expires: 1790000000,
                    mac: "52:54:00:aa:bb:cc".into(),
                    ip: "10.9.0.10".parse().unwrap(),
                },
                Lease {
                    expires: 1790003600,
                    mac: "52:54:00:aa:bb:cd".into(),
                    ip: "10.9.0.11".parse().unwrap(),
                },
            ]
        );
        assert!(parse_leases("").is_empty());
    }

    #[test]
    fn the_query_log_rotates_only_past_the_cap() {
        assert!(!should_rotate(0));
        assert!(!should_rotate(LOG_MAX_BYTES));
        assert!(should_rotate(LOG_MAX_BYTES + 1));
        assert!(should_rotate(LOG_MAX_BYTES * 2));
    }

    /// The pid file is bookkeeping, not proof: a live process whose cmdline
    /// does not name our conf must never be signalled (SIGTERM on stop,
    /// SIGUSR2 on rotate). pid 1 and a garbage pid fail the same check.
    #[test]
    fn a_pid_that_is_not_our_dnsmasq_is_never_trusted() {
        let dir = std::env::temp_dir().join(format!("hostnet-pid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = host_in(&dir);
        let pid_path = dir.join("dnsmasq-n8.pid");
        std::fs::write(&pid_path, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(
            host.verified_pid("n8"),
            None,
            "this live test process does not run the network's conf"
        );
        assert!(!host.dnsmasq_running("n8"));
        std::fs::write(&pid_path, "1\n").unwrap();
        assert_eq!(host.verified_pid("n8"), None);
        std::fs::write(&pid_path, "not-a-pid\n").unwrap();
        assert_eq!(host.verified_pid("n8"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A rename must leave a fresh log to receive new lookups. With no dnsmasq
    /// running there is no pid to signal; the helper above pins that check.
    #[test]
    fn rotating_the_query_log_moves_it_and_starts_a_fresh_one() {
        let dir = std::env::temp_dir().join(format!("hostnet-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = host_in(&dir);
        let log = host.log_path("n9");
        std::fs::File::create(&log)
            .unwrap()
            .set_len(LOG_MAX_BYTES + 1)
            .unwrap(); // sparse: no 64 MiB is written
        host.rotate_log("n9");
        assert!(dir.join("dnsmasq-n9.log.1").exists());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_forgets_every_file_of_a_network() {
        let dir = std::env::temp_dir().join(format!("hostnet-remove-{}", std::process::id()));
        let host = host_in(&dir);
        host.save_ports("n7", &["vnet0".into()]).unwrap();
        std::fs::write(dir.join("dnsmasq-n7.conf"), "x").unwrap();
        std::fs::write(dir.join("dnsmasq-n7.log"), "x").unwrap();
        host.remove("n7");
        assert!(host.read_ports("n7").is_empty());
        assert!(!dir.join("dnsmasq-n7.conf").exists());
        assert!(!dir.join("dnsmasq-n7.log").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
