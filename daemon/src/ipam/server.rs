//! The network manager: the store says what each network is, `hostnet` runs
//! its bridge and dnsmasq, and this keeps both in step with the guests.
//!
//! A supervisor runs every few seconds. It reads every defined guest's NICs,
//! gives each guest appearing on a managed network the lowest free address (a
//! permanent reservation), and starts, restarts or stops each network's
//! dnsmasq to match. Nothing is read from libvirt's network driver: these
//! networks do not exist there, so there is nothing to take over or prune.
//! `Mode::Bridge` only verifies the operator's bridge and never runs a
//! dnsmasq of ours.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::store::{self, Net, Reservation, Store};
use crate::hostnet::{self, HostNet};
use crate::libvirt::with_conn;

const SUPERVISE_SECS: u64 = 5;

/// One guest interface on a network. What a reservation is tied to, and the
/// name its DNS record answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestNic {
    /// libvirt UUID of the guest.
    pub uuid: String,
    /// What the user calls it: its `<title>`, else libvirt's name.
    pub name: String,
    /// The Linux bridge it is plugged into. Empty while the guest is still
    /// defined on a libvirt network, which this host no longer has.
    pub bridge: String,
    /// Whether the guest is running now. A network delete is refused for any
    /// defined guest's NIC, running or not; this records the live state.
    pub running: bool,
}

/// Shared by the supervisor and the API.
#[derive(Clone)]
pub struct Ipam {
    pub store: Arc<Mutex<Store>>,
    path: PathBuf,
    /// MAC -> the guest NIC that has it, for every defined guest. Refreshed by
    /// the supervisor; a MAC no guest here has never gets a DHCP address.
    /// Crate-visible so a test can seed a snapshot without libvirt.
    pub(crate) guests: Arc<Mutex<HashMap<String, GuestNic>>>,
    pub host: HostNet,
    /// What guests connect to, for the access log. See `flows`.
    pub flows: super::flows::Flows,
    /// How far each network's dnsmasq query log has been read, and the queries
    /// still waiting for an answer. See `dnslog`.
    dns_logs: Arc<Mutex<HashMap<String, super::dnslog::Tail>>>,
    /// Set while the guest snapshot is failing, so an outage is logged once
    /// rather than every five seconds. Cleared (and logged once) on recovery.
    snapshot_stale: Arc<AtomicBool>,
}

impl Ipam {
    pub fn load(path: PathBuf, data_dir: PathBuf) -> Result<Self, String> {
        Ok(Self {
            store: Arc::new(Mutex::new(store::load(&path)?)),
            path,
            guests: Default::default(),
            host: HostNet::new(data_dir),
            flows: Default::default(),
            dns_logs: Default::default(),
            snapshot_stale: Default::default(),
        })
    }

    /// Whose guest an address is: the reservation holding it, on whichever
    /// managed network it falls in. None for the host, gateways, and anything
    /// not reserved (every guest address is, since reservations are permanent).
    pub fn owner(&self, ip: Ipv4Addr) -> Option<super::flows::Owner> {
        let s = self.lock();
        let (name, net) = s
            .networks
            .iter()
            .find(|(_, n)| n.contains_host(ip) && ip != n.gateway)?;
        let r = net.reservation_for_ip(ip)?;
        let guest = self.guest_of(&r.mac);
        Some(super::flows::Owner {
            network: name.clone(),
            gateway: net.gateway,
            mac: r.mac.clone(),
            vm: r
                .vm
                .clone()
                .or_else(|| guest.as_ref().map(|g| g.uuid.clone())),
            vm_name: guest.map(|g| g.name),
        })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("ipam store poisoned")
    }

    /// Persist the store. Called with the change already made.
    pub fn save(&self, s: &Store) -> Result<(), String> {
        store::save(&self.path, s)
    }

    /// Change one network's state and persist it, as one step.
    pub fn update<T>(
        &self,
        net: &str,
        f: impl FnOnce(&mut Net) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut s = self.lock();
        let n = s
            .networks
            .get_mut(net)
            .ok_or_else(|| format!("network '{net}' is not managed here"))?;
        let out = f(n)?;
        self.save(&s)?;
        Ok(out)
    }

    pub fn guest_of(&self, mac: &str) -> Option<GuestNic> {
        self.guests
            .lock()
            .expect("guests poisoned")
            .get(mac)
            .cloned()
    }

    /// The stored key a path segment names: the key itself (a network's UUID),
    /// else the title it displays, for a link by name. Empty names nothing, and
    /// an unknown name answers `None` so a handler can say "not found" rather
    /// than act on a typo.
    pub fn resolve_key(&self, key: &str) -> Option<String> {
        if key.is_empty() {
            return None;
        }
        let s = self.lock();
        if s.networks.contains_key(key) {
            return Some(key.to_string());
        }
        s.networks
            .iter()
            .find(|(_, n)| n.title == key)
            .map(|(id, _)| id.clone())
    }

    /// The bridge a guest attaches to for the network named by `key`: its
    /// stored id (a UUID) or its display name. The bridge is derived, never
    /// stored, so it always matches what the host runs.
    pub fn bridge_for(&self, key: &str) -> Option<String> {
        let id = self.resolve_key(key)?;
        let s = self.lock();
        s.networks.get(&id).map(|net| hostnet::bridge_for(net, &id))
    }

    /// The stored key of the network whose bridge is `bridge`: a guest's NIC
    /// names the bridge, everything else here names the network. Two
    /// `Mode::Bridge` networks may borrow one operator bridge, so this answers
    /// the lowest stored key (the store is a `BTreeMap`) to be deterministic:
    /// the same guest always reports the same network.
    pub fn network_for_bridge(&self, bridge: &str) -> Option<String> {
        if bridge.is_empty() {
            return None;
        }
        let s = self.lock();
        s.networks
            .iter()
            .find(|(id, n)| hostnet::bridge_for(n, id) == bridge)
            .map(|(id, _)| id.clone())
    }

    /// Whether this host is running the network now: its bridge exists and,
    /// for a network that serves addresses, its dnsmasq is up.
    pub fn serving(&self, id: &str) -> bool {
        self.lock()
            .networks
            .get(id)
            .is_some_and(|net| crate::hostnet::is_active(net, id))
    }

    /// The reservations dnsmasq may hand a DHCP address to: only reservations
    /// whose MAC belongs to a guest NIC on *this* network's bridge. A guest
    /// moved to another network must lose its address here, or two networks
    /// would answer for it. Every other reservation still answers DNS.
    pub fn dhcp_reservations(&self, net: &Net, id: &str) -> Vec<Reservation> {
        let guests = self.guests.lock().expect("guests poisoned");
        let bridge = hostnet::bridge_for(net, id);
        net.reservations
            .iter()
            .filter(|r| guests.get(&r.mac).is_some_and(|g| g.bridge == bridge))
            .cloned()
            .collect()
    }

    pub fn dhcp_macs(&self, net: &Net, id: &str) -> HashSet<String> {
        self.dhcp_reservations(net, id)
            .into_iter()
            .map(|r| r.mac)
            .collect()
    }

    /// Every guest NIC the last snapshot saw, for an in-use check.
    pub fn nics(&self) -> Vec<GuestNic> {
        self.guests
            .lock()
            .expect("guests poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// Run the supervisor loop. The first pass is immediate, so a network that was
/// brought down while the daemon was not running is noticed at once. `startup`
/// is the caller's job (main runs it once, before the API is served).
pub async fn supervise(ipam: Ipam) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(SUPERVISE_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = supervise_once(&ipam).await {
            tracing::warn!("ipam: {e}");
        }
    }
}

/// Everything a restart must do before the API takes a request: read the
/// guests, bring up the networks that are meant to be up, give their guests
/// an address, and rebuild the firewall table from the store (nftables does
/// not survive a reboot).
pub async fn startup(ipam: Ipam) -> Result<(), String> {
    // libvirtd may be down (or the host mid-boot): log and carry on, because
    // network lifecycle must not depend on it. A failed *store* write is the
    // one thing that is fatal — it would lose the reservations just made.
    let guests_read = match snapshot(&ipam).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("ipam: reading guests: {e}");
            ipam.snapshot_stale.store(true, Ordering::Relaxed);
            false
        }
    };
    let nets: Vec<(String, Net)> = ipam
        .lock()
        .networks
        .iter()
        .map(|(id, n)| (id.clone(), n.clone()))
        .collect();
    for (id, net) in &nets {
        if !net.autostart || hostnet::bridge_up(net, id) {
            continue;
        }
        let macs = ipam.dhcp_macs(net, id);
        // One broken network must not keep the others, or the daemon, down:
        // the supervisor retries it on its next pass.
        if let Err(e) = ipam.host.start(net, id, &net.reservations, &macs).await {
            tracing::warn!("ipam: cannot start network {id}: {e}");
        }
    }
    // Without a guest snapshot every reservation would look orphaned, and
    // reconcile would delete them: skip it and let the first good pass do it.
    if guests_read {
        reconcile(&ipam)?;
    }
    // nftables does not survive a reboot. A missing nft or an unreachable
    // libvirtd must not stop the daemon from serving its networks.
    match crate::firewall::load(std::path::Path::new(crate::firewall::FIREWALL_PATH)) {
        Ok(fw) => {
            if let Err(e) = crate::api::firewall::apply_store(&ipam, &fw).await {
                tracing::warn!("ipam: applying the firewall at startup: {e}");
            }
        }
        Err(e) => tracing::warn!("ipam: reading the firewall store: {e}"),
    }
    Ok(())
}

/// One supervisor pass: refresh the guests, settle the reservations, and match
/// dnsmasq to each network's state.
pub async fn supervise_once(ipam: &Ipam) -> Result<(), String> {
    // A libvirtd outage must not stop the networks: keep the last snapshot,
    // skip reconcile (it would prune every reservation from stale or empty
    // data), and still do the per-network dnsmasq, lease and log work. The
    // failure is logged once, not every five seconds.
    let fresh = match snapshot(ipam).await {
        Ok(()) => {
            if ipam.snapshot_stale.swap(false, Ordering::Relaxed) {
                tracing::info!("ipam: guest snapshot is readable again; reconciling reservations");
            }
            true
        }
        Err(e) => {
            if !ipam.snapshot_stale.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "ipam: reading guests: {e}; keeping the last snapshot and leaving reservations alone"
                );
            }
            false
        }
    };
    if fresh {
        reconcile(ipam)?;
    }
    let nets: Vec<(String, Net)> = ipam
        .lock()
        .networks
        .iter()
        .map(|(id, n)| (id.clone(), n.clone()))
        .collect();
    for (id, net) in nets {
        if !hostnet::bridge_up(&net, &id) {
            // A stopped network must not keep answering DHCP or DNS. Stopping
            // an idle dnsmasq is a no-op, so this is safe every pass; a failure
            // is logged rather than aborting the other networks' pass.
            if let Err(e) = ipam.host.stop_dnsmasq(&id).await {
                tracing::warn!("ipam: stopping dnsmasq for {id}: {e}");
            }
            continue;
        }
        // A bridge network has no address of ours to serve.
        if !net.is_served() {
            continue;
        }
        let macs = ipam.dhcp_macs(&net, &id);
        // dnsmasq reads its config once at startup, so a changed reservation
        // needs a restart, not just a rewritten file. Every named reservation
        // answers DNS; only the MACs of guests on this bridge are DHCP.
        let want = ipam.host.dnsmasq_conf(&net, &id, &net.reservations, &macs);
        let have = std::fs::read_to_string(ipam.host.conf_path(&id)).unwrap_or_default();
        let out = if !ipam.host.dnsmasq_running(&id) {
            ipam.host
                .start_dnsmasq(&net, &id, &net.reservations, &macs)
                .await
        } else if want != have {
            ipam.host
                .restart_dnsmasq(&net, &id, &net.reservations, &macs)
                .await
        } else {
            Ok(())
        };
        if let Err(e) = out {
            tracing::warn!("ipam: dnsmasq for {id}: {e}");
        }
        // The query log is bounded here, not only when dnsmasq restarts: a
        // server that runs for weeks would otherwise never be pruned. Read it
        // before rotating, so a pass's lookups are not dropped with the file.
        record_dns(ipam, &id);
        ipam.host.rotate_log(&id);
        record_leases(ipam, &id)?;
    }
    Ok(())
}

/// Turn the DNS lookups dnsmasq answered since the last pass into access-log
/// rows, and remember the names so a connection that follows finds them.
fn record_dns(ipam: &Ipam, id: &str) {
    let now = now_ms();
    let path = ipam.host.log_path(id);
    let answers = ipam
        .dns_logs
        .lock()
        .expect("dns logs poisoned")
        .entry(id.to_string())
        .or_default()
        .read(&path, now);
    for a in answers {
        if a.first {
            let lookup = super::flows::Lookup {
                client: a.client,
                name: a.name,
                answers: vec![a.ip],
            };
            ipam.flows.record_lookup(&lookup, ipam.owner(a.client), now);
        } else {
            ipam.flows.record_name(a.client, a.ip, &a.name, now);
        }
    }
}

/// Fold dnsmasq's leases into the reservations' `seen_ms`: a lease expiring at
/// `T` with a lease time of `L` was last renewed at `T - L`, which is what the
/// network page shows as "last seen". A lease that is never renewed leaves the
/// old timestamp alone, so the field means what it says.
fn record_leases(ipam: &Ipam, id: &str) -> Result<(), String> {
    let leases = ipam.host.read_leases(id);
    if leases.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut s = ipam.lock();
    let mut changed = false;
    {
        let Some(net) = s.networks.get_mut(id) else {
            return Ok(());
        };
        for lease in &leases {
            // 0 is an infinite lease: it says nothing about a renewal.
            if lease.expires == 0 {
                continue;
            }
            let seen = lease
                .expires
                .saturating_sub(u64::from(net.lease_secs))
                .saturating_mul(1000)
                .min(now);
            if let Some(r) = net
                .reservations
                .iter_mut()
                .find(|r| r.mac == lease.mac && r.ip == lease.ip)
                && r.seen_ms != seen
            {
                r.seen_ms = seen;
                changed = true;
            }
        }
    }
    if changed {
        ipam.save(&s)?;
    }
    Ok(())
}

/// Read the guests and keep only what this daemon acts on: the MACs, the
/// bridge each is plugged into, and who it is. A failed read is an error, not
/// an empty host: forgetting every guest would prune every reservation.
async fn snapshot(ipam: &Ipam) -> Result<(), String> {
    let guests = snapshot_guests().await?;
    *ipam.guests.lock().expect("guests poisoned") = guests;
    Ok(())
}

async fn snapshot_guests() -> Result<HashMap<String, GuestNic>, String> {
    with_conn(|c| {
        let mut out = HashMap::new();
        for d in crate::libvirt::all_domains(c, 0) {
            let (Ok(uuid), Ok(name)) = (d.get_uuid_string(), d.get_name()) else {
                continue;
            };
            let running = d.is_active().unwrap_or(false);
            let Ok(x) = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE) else {
                continue;
            };
            // The display name, not the libvirt name: a guest the forms made
            // is named by its UUID, so using `name` here put a UUID in the
            // DHCP hostname and the DNS answer.
            let guest_name = crate::libvirt::title_of(&x).unwrap_or(name);
            for (mac, kind, source) in interfaces(&x) {
                // Guests are being moved from libvirt networks to bridges.
                // There is no libvirt network on this host any more, so such
                // an interface has no bridge here: it matches no network and
                // is given no address until it is attached to one. It is
                // still recorded, so its reservations are not pruned.
                let bridge = if kind == "bridge" {
                    source
                } else {
                    if !source.is_empty() {
                        tracing::warn!(
                            "ipam: guest {}: interface on libvirt network '{}' has no bridge; not served",
                            guest_name,
                            source
                        );
                    }
                    String::new()
                };
                out.insert(
                    mac,
                    GuestNic {
                        uuid: uuid.clone(),
                        name: guest_name.clone(),
                        bridge,
                        running,
                    },
                );
            }
        }
        Ok(out)
    })
    .await
}

/// Every interface in a domain XML as `(mac, type, bridge or network name)`.
/// The source is `<source bridge=...>` for a bridge interface and
/// `<source network=...>` for a libvirt network one.
fn interfaces(xml: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let end = block.find("</interface>").unwrap_or(block.len());
        let b = &block[..end];
        if let Some(mac) = b.find("<mac").and_then(|m| {
            let tag = &b[m..];
            let e = tag.find('>')?;
            crate::disks::attr(&tag[..=e], "address").and_then(store::normalize_mac)
        }) {
            let open_end = b.find('>').map_or(b.len(), |e| e + 1);
            let kind = crate::disks::attr(&b[..open_end], "type").unwrap_or("network");
            let source = crate::disks::attr(b, "bridge")
                .or_else(|| crate::disks::attr(b, "network"))
                .unwrap_or_default();
            out.push((mac, kind.to_string(), source.to_string()));
        }
        rest = &block[end.saturating_add("</interface>".len()).min(block.len())..];
    }
    out
}

/// Make the reservations match the guests and the networks: every guest NIC on
/// a network's bridge gets an address, a reservation whose guest is gone is
/// dropped, and a manual reservation whose MAC belongs to a guest is tied to
/// it so deleting the guest frees the address. No network is ever removed here:
/// the store is the source of truth, and only the API deletes.
fn reconcile(ipam: &Ipam) -> Result<(), String> {
    let guests = ipam.guests.lock().expect("guests poisoned").clone();
    let uuids: HashSet<String> = guests.values().map(|g| g.uuid.clone()).collect();
    let mut s = ipam.lock();
    let before = s.clone();
    for (id, net) in s.networks.iter_mut() {
        // A network this host does not serve (an operator's bridge, or an
        // isolated segment with no subnet) has no range to allocate from.
        if !net.is_served() {
            continue;
        }
        let bridge = hostnet::bridge_for(net, id);
        for (mac, nic) in &guests {
            if nic.bridge != bridge {
                continue;
            }
            if let Err(e) = net.ensure_reservation(mac, &nic.uuid, &nic.name) {
                tracing::warn!("ipam: {id}: {e}");
            }
        }
        net.reservations.retain(|r| match &r.vm {
            Some(u) => uuids.contains(u),
            None => true,
        });
        for r in &mut net.reservations {
            if r.vm.is_none()
                && let Some(nic) = guests.get(&r.mac)
            {
                r.vm = Some(nic.uuid.clone());
            }
        }
    }
    if *s != before {
        ipam.save(&s)?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::interfaces;

    #[test]
    fn a_guest_interface_yields_its_mac_type_and_source() {
        let x = "<domain><devices>
          <interface type='network'><mac address='52:54:00:AA:00:01'/><source network='a'/></interface>
          <interface type='bridge'><source bridge='br0'/><mac address=\"52:54:00:aa:00:02\"/></interface>
          <interface type='network'><source network='a'/></interface>
          <interface type='bridge'><mac address='52:54:00:aa:00:03'/></interface>
        </devices></domain>";
        assert_eq!(
            interfaces(x),
            vec![
                ("52:54:00:aa:00:01".into(), "network".into(), "a".into()),
                ("52:54:00:aa:00:02".into(), "bridge".into(), "br0".into()),
                ("52:54:00:aa:00:03".into(), "bridge".into(), String::new()),
            ],
            "an interface with no MAC is skipped"
        );
    }

    #[test]
    fn a_domain_without_interfaces_yields_nothing() {
        assert!(interfaces("<domain><devices/></domain>").is_empty());
    }

    /// A network is reached by its stored id or its title, and a guest's NIC is
    /// read back through the bridge it names. A bridge network borrows the
    /// operator's bridge, so it is found by that name too.
    #[test]
    fn a_network_is_found_by_key_title_or_bridge() {
        let dir = std::env::temp_dir().join(format!("ipam-bridges-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = super::Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        {
            let mut s = ipam.lock();
            s.networks.insert(
                id.into(),
                crate::ipam::store::import(
                    "<network><title>lab</title><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
                )
                .unwrap(),
            );
            ipam.save(&s).unwrap();
        }
        let bridge = crate::hostnet::bridge_name(id);
        assert_eq!(ipam.resolve_key(id).as_deref(), Some(id));
        assert_eq!(ipam.resolve_key("lab").as_deref(), Some(id));
        assert_eq!(
            ipam.resolve_key("").as_deref(),
            None,
            "an empty key names nothing"
        );
        assert_eq!(ipam.resolve_key("nope").as_deref(), None);
        assert_eq!(ipam.bridge_for(id).as_deref(), Some(bridge.as_str()));
        assert_eq!(ipam.bridge_for("lab").as_deref(), Some(bridge.as_str()));
        assert_eq!(ipam.bridge_for(""), None, "an empty key is not a title");
        assert_eq!(ipam.bridge_for("nope"), None);
        assert_eq!(ipam.network_for_bridge(&bridge).as_deref(), Some(id));
        assert_eq!(ipam.network_for_bridge("br0"), None);
        {
            let mut s = ipam.lock();
            let net = s.networks.get_mut(id).unwrap();
            net.mode = super::store::Mode::Bridge;
            net.bridge = "br0".into();
        }
        assert_eq!(ipam.bridge_for("lab").as_deref(), Some("br0"));
        assert_eq!(ipam.network_for_bridge("br0").as_deref(), Some(id));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two operator-bridge networks may borrow one bridge. The answer must not
    /// depend on map iteration luck: the lowest stored key wins, so a guest's
    /// NIC reports the same network on every read.
    #[test]
    fn a_shared_bridge_answers_the_lowest_key() {
        let dir = std::env::temp_dir().join(format!("ipam-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = super::Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        // Inserted out of order on purpose: the store keeps them sorted.
        for id in [
            "cccccccc-0000-4000-8000-000000000000",
            "aaaaaaaa-0000-4000-8000-000000000000",
        ] {
            let mut net = crate::ipam::store::import(
                "<network><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
            )
            .unwrap();
            net.mode = super::store::Mode::Bridge;
            net.bridge = "br0".into();
            ipam.lock().networks.insert(id.into(), net);
        }
        assert_eq!(
            ipam.network_for_bridge("br0").as_deref(),
            Some("aaaaaaaa-0000-4000-8000-000000000000")
        );
        assert_eq!(
            ipam.bridge_for("aaaaaaaa-0000-4000-8000-000000000000")
                .as_deref(),
            Some("br0")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dhcp_is_scoped_to_the_guests_on_that_bridges_network() {
        let dir = std::env::temp_dir().join(format!("ipam-dhcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = super::Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let mut net = crate::ipam::store::import(
            "<network><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
        )
        .unwrap();
        for (i, mac) in ["52:54:00:aa:bb:01", "52:54:00:aa:bb:02"]
            .iter()
            .enumerate()
        {
            net.reservations.push(crate::ipam::store::Reservation {
                mac: (*mac).into(),
                ip: format!("10.9.0.{}", i + 2).parse().unwrap(),
                name: Some(format!("guest-{i}")),
                dynamic: true,
                vm: None,
                seen_ms: 0,
            });
        }
        ipam.lock().networks.insert(id.into(), net.clone());
        for (mac, bridge) in [
            ("52:54:00:aa:bb:01", crate::hostnet::bridge_for(&net, id)),
            // Same host, different network: a guest moved here, not there.
            ("52:54:00:aa:bb:02", "vbrother".to_string()),
        ] {
            ipam.guests.lock().unwrap().insert(
                mac.into(),
                super::GuestNic {
                    uuid: format!("u-{mac}"),
                    name: mac.into(),
                    bridge,
                    running: false,
                },
            );
        }
        let macs = ipam.dhcp_macs(&net, id);
        assert!(macs.contains("52:54:00:aa:bb:01"));
        assert!(
            !macs.contains("52:54:00:aa:bb:02"),
            "a guest on another bridge is not handed an address here"
        );
        assert_eq!(ipam.dhcp_reservations(&net, id).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_lease_sets_the_reservations_last_seen_time() {
        let dir = std::env::temp_dir().join(format!("ipam-leases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = super::Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let mut net = crate::ipam::store::import(
            "<network><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
        )
        .unwrap();
        net.reservations.push(crate::ipam::store::Reservation {
            mac: "52:54:00:aa:bb:cc".into(),
            ip: "10.9.0.2".parse().unwrap(),
            name: Some("web".into()),
            dynamic: true,
            vm: None,
            seen_ms: 0,
        });
        {
            let mut s = ipam.lock();
            s.networks.insert(id.into(), net);
            ipam.save(&s).unwrap();
        }
        // dnsmasq's lease file: expiry, MAC, address, then what we ignore.
        std::fs::write(
            dir.join(format!("dnsmasq-{id}.leases")),
            "1790000000 52:54:00:aa:bb:cc 10.9.0.2 web *\n",
        )
        .unwrap();
        super::record_leases(&ipam, id).unwrap();
        let seen = ipam.lock().networks[id].reservations[0].seen_ms;
        assert_eq!(
            seen,
            (1790000000 - 3600) * 1000,
            "the renewal is the expiry minus the lease time, in ms"
        );
        // Reading the same lease again changes nothing.
        super::record_leases(&ipam, id).unwrap();
        assert_eq!(ipam.lock().networks[id].reservations[0].seen_ms, seen);
        let _ = std::fs::remove_dir_all(dir);
    }
}
