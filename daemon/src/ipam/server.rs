//! The running side of IPAM: one DHCP and one DNS listener per managed,
//! active network, kept in step with libvirt.
//!
//! A supervisor runs every few seconds: it takes over any network libvirt would
//! start dnsmasq for, starts listeners for managed networks that are up, stops
//! them for networks that went down, and forgets reservations whose guest is
//! gone. Doing this by polling rather than libvirt events keeps it simple, and
//! a network started with `virsh` is picked up within seconds all the same.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use virt::network::Network;

use super::store::{self, Net, Store};
use super::{dhcp, dns, xml};
use crate::firewall;
use crate::libvirt::with_conn;

const SUPERVISE_SECS: u64 = 5;
const FORWARD_TIMEOUT_MS: u64 = 3000;

/// Shared by the servers and the API.
#[derive(Clone)]
pub struct Ipam {
    pub store: Arc<Mutex<Store>>,
    path: PathBuf,
    /// MAC -> (domain uuid, domain name), for every defined guest. Refreshed
    /// by the supervisor; the DHCP server serves only MACs listed here.
    guests: Arc<Mutex<HashMap<String, (String, String)>>>,
    running: Arc<Mutex<HashMap<String, Listeners>>>,
    /// What guests connect to, for the access log. See `flows`.
    pub flows: super::flows::Flows,
}

struct Listeners {
    bridge: String,
    gateway: Ipv4Addr,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Ipam {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        Ok(Self {
            store: Arc::new(Mutex::new(store::load(&path)?)),
            path,
            guests: Default::default(),
            running: Default::default(),
            flows: Default::default(),
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
            vm: r.vm.clone().or_else(|| guest.as_ref().map(|g| g.0.clone())),
            vm_name: guest.map(|g| g.1),
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

    pub fn guest_of(&self, mac: &str) -> Option<(String, String)> {
        self.guests
            .lock()
            .expect("guests poisoned")
            .get(mac)
            .cloned()
    }

    pub fn serving(&self, net: &str) -> bool {
        self.running
            .lock()
            .expect("listeners poisoned")
            .contains_key(net)
    }
}

/// Run the supervisor forever.
pub async fn supervise(ipam: Ipam) {
    // First pass straight away: guests on a managed network have no DHCP or
    // DNS until it runs, so a daemon restart should not add five seconds.
    if let Err(e) = supervise_once(&ipam).await {
        tracing::warn!("ipam: {e}");
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(SUPERVISE_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = supervise_once(&ipam).await {
            tracing::warn!("ipam: {e}");
        }
    }
}

/// What libvirt currently has: every network's inactive XML and state, and
/// every guest's MACs.
struct Snapshot {
    /// name -> (inactive xml, active, bridge)
    nets: Vec<(String, String, bool, Option<String>)>,
    guests: HashMap<String, (String, String)>,
}

async fn snapshot() -> Result<Snapshot, String> {
    with_conn(|c| {
        let mut nets = Vec::new();
        for n in c.list_all_networks(0)? {
            let name = n.get_name().unwrap_or_default();
            let xml = n.get_xml_desc(virt::sys::VIR_NETWORK_XML_INACTIVE)?;
            let active = n.is_active().unwrap_or(false);
            let bridge = n.get_bridge_name().ok();
            nets.push((name, xml, active, bridge));
        }
        let mut guests = HashMap::new();
        for d in c.list_all_domains(0)? {
            let (Ok(uuid), Ok(name)) = (d.get_uuid_string(), d.get_name()) else {
                continue;
            };
            let Ok(x) = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE) else {
                continue;
            };
            for mac in macs(&x) {
                guests.insert(mac, (uuid.clone(), name.clone()));
            }
        }
        Ok(Snapshot { nets, guests })
    })
    .await
}

fn macs(domain_xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = domain_xml;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let end = block.find("</interface>").unwrap_or(block.len());
        if let Some(m) = block[..end].find("<mac")
            && let Some(a) =
                crate::disks::attr(&block[m..m + block[m..].find('>').unwrap_or(0)], "address")
            && let Some(mac) = store::normalize_mac(a)
        {
            out.push(mac);
        }
        rest = &block[end.max(1)..];
    }
    out
}

pub async fn supervise_once(ipam: &Ipam) -> Result<(), String> {
    let snap = snapshot().await?;
    // A guest list that failed to read must not look like "every guest was
    // deleted": the snapshot errors instead, and nothing is pruned.
    *ipam.guests.lock().expect("guests poisoned") = snap.guests.clone();

    for (name, x, active, _) in &snap.nets {
        if xml::needs_takeover(x) {
            if let Err(e) = take_over(ipam, name, x, *active).await {
                tracing::warn!("ipam: taking over network {name}: {e}");
            }
        } else if xml::is_managed(x) && !ipam.lock().networks.contains_key(name) {
            // Managed in libvirt but missing from the store (store deleted,
            // or a network defined with DNS off by hand): adopt its subnet.
            if let Some(n) = store::import(x) {
                let mut s = ipam.lock();
                s.networks.insert(name.clone(), n);
                ipam.save(&s)?;
            }
        }
    }

    // Forget networks that no longer exist, and reservations whose guest was
    // deleted. Manual reservations for a MAC no guest has are the user's and
    // stay; only ones tied to a guest go with it.
    {
        let names: HashSet<&str> = snap.nets.iter().map(|n| n.0.as_str()).collect();
        let uuids: HashSet<&str> = snap.guests.values().map(|g| g.0.as_str()).collect();
        let mut s = ipam.lock();
        let before = s.clone();
        s.networks.retain(|n, _| names.contains(n.as_str()));
        for n in s.networks.values_mut() {
            n.reservations.retain(|r| match &r.vm {
                Some(u) => uuids.contains(u.as_str()),
                None => true,
            });
            // A manual reservation whose MAC now belongs to a guest is tied to
            // it, so deleting the guest frees the address.
            for r in &mut n.reservations {
                if r.vm.is_none()
                    && let Some((u, _)) = snap.guests.get(&r.mac)
                {
                    r.vm = Some(u.clone());
                }
            }
        }
        if *s != before {
            ipam.save(&s)?;
        }
    }

    // Let port forwards past libvirt's own per-network filter, which rejects
    // the first packet of a forwarded connection because it is NEW rather than
    // related/established. The accept has to sit inside libvirt's chain, so it
    // is re-asserted every pass: starting or restarting a network, and a
    // libvirtd restart, all make libvirt rewrite that chain without it.
    let subnets: Vec<String> = {
        let s = ipam.lock();
        snap.nets
            .iter()
            .filter(|(name, x, _, _)| xml::is_managed(x) && s.networks.contains_key(name))
            .filter_map(|(name, _, _, _)| {
                let n = &s.networks[name];
                firewall::network_prefix(&format!("{}/{}", n.gateway, n.prefix))
            })
            .collect()
    };
    for subnet in subnets {
        if let Err(e) = firewall::allow_forwarded(&subnet).await {
            tracing::warn!("ipam: {e}");
        }
    }

    // Start and stop listeners to match.
    let want: HashMap<String, (String, Ipv4Addr)> = {
        let s = ipam.lock();
        snap.nets
            .iter()
            .filter(|(name, x, active, bridge)| {
                *active && bridge.is_some() && xml::is_managed(x) && s.networks.contains_key(name)
            })
            .map(|(name, _, _, bridge)| {
                (
                    name.clone(),
                    (bridge.clone().unwrap_or_default(), s.networks[name].gateway),
                )
            })
            .collect()
    };
    let mut running = ipam.running.lock().expect("listeners poisoned");
    running.retain(|name, l| {
        want.get(name)
            .is_some_and(|(b, g)| *b == l.bridge && *g == l.gateway)
    });
    for (name, (bridge, gateway)) in want {
        if running.contains_key(&name) {
            continue;
        }
        match start(ipam, &name, &bridge, gateway) {
            Ok(tasks) => {
                tracing::info!("ipam: serving DHCP and DNS on {bridge} ({gateway}) for {name}");
                running.insert(
                    name,
                    Listeners {
                        bridge,
                        gateway,
                        tasks,
                    },
                );
            }
            Err(e) => tracing::warn!("ipam: cannot serve {name} on {bridge}: {e}"),
        }
    }
    Ok(())
}

/// Move one network from dnsmasq to us: store its IPAM, redefine it without,
/// and if it was running restart it and plug its guests back in.
async fn take_over(ipam: &Ipam, name: &str, x: &str, active: bool) -> Result<(), String> {
    let Some(imported) = store::import(x) else {
        return Ok(());
    };
    {
        let mut s = ipam.lock();
        let merged = xml::merge(s.networks.get(name), x).unwrap_or(imported);
        s.networks.insert(name.to_string(), merged);
        ipam.save(&s)?;
    }
    let stripped = xml::strip(x);
    let name = name.to_string();
    let reattached = with_conn(move |c| {
        let n = Network::define_xml(c, &stripped)?;
        if !active {
            return Ok(0);
        }
        // The change takes effect on restart. Guests keep their addresses
        // (they are all reservations now) and are plugged straight back in.
        if let Ok(old) = crate::libvirt::network(c, &name)
            && old.is_active().unwrap_or(false)
        {
            old.destroy()?;
        }
        n.create()?;
        Ok(crate::api::networks::reattach_taps(c, &n))
    })
    .await?;
    tracing::info!("ipam: took network over from dnsmasq ({reattached} guest link(s) re-plugged)");
    Ok(())
}

/// Bind the DHCP and DNS sockets for one bridge and spawn their loops.
fn start(
    ipam: &Ipam,
    net: &str,
    bridge: &str,
    gateway: Ipv4Addr,
) -> Result<Vec<JoinHandle<()>>, String> {
    let dhcp_sock = udp_on_device(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, dhcp::SERVER_PORT)),
        bridge,
        true,
    )?;
    let dns_udp = udp_on_device(SocketAddr::from((gateway, dns::PORT)), bridge, false)?;
    let dns_tcp = tcp_on(SocketAddr::from((gateway, dns::PORT)))?;
    let (a, b, c) = (ipam.clone(), ipam.clone(), ipam.clone());
    let (n1, n2, n3) = (net.to_string(), net.to_string(), net.to_string());
    Ok(vec![
        tokio::spawn(async move { dhcp_loop(a, n1, dhcp_sock).await }),
        tokio::spawn(async move { dns_udp_loop(b, n2, dns_udp).await }),
        tokio::spawn(async move { dns_tcp_loop(c, n3, dns_tcp).await }),
    ])
}

fn udp_on_device(addr: SocketAddr, dev: &str, broadcast: bool) -> Result<UdpSocket, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s =
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    s.set_reuse_address(true).map_err(|e| e.to_string())?;
    // One DHCP socket per bridge, all on 0.0.0.0:67: binding each to its
    // device is what lets several coexist, and it keeps a broadcast from one
    // guest network from being answered by another network's server.
    s.bind_device(Some(dev.as_bytes()))
        .map_err(|e| format!("bind to {dev}: {e}"))?;
    if broadcast {
        s.set_broadcast(true).map_err(|e| e.to_string())?;
    }
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    s.bind(&addr.into())
        .map_err(|e| format!("bind {addr}: {e}"))?;
    UdpSocket::from_std(s.into()).map_err(|e| e.to_string())
}

fn tcp_on(addr: SocketAddr) -> Result<tokio::net::TcpListener, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s =
        Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).map_err(|e| e.to_string())?;
    s.set_reuse_address(true).map_err(|e| e.to_string())?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    s.bind(&addr.into())
        .map_err(|e| format!("bind {addr}: {e}"))?;
    s.listen(64).map_err(|e| e.to_string())?;
    tokio::net::TcpListener::from_std(s.into()).map_err(|e| e.to_string())
}

// ------------------------------------------------------------ DHCP

async fn dhcp_loop(ipam: Ipam, net: String, sock: UdpSocket) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, _from)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        let Some(p) = dhcp::parse(&buf[..n]) else {
            continue;
        };
        let Some(snapshot) = ipam.lock().networks.get(&net).cloned() else {
            continue;
        };
        let now = now_ms();
        let in_use = |ip: Ipv4Addr| ip == snapshot.gateway;
        let (reply, change) = dhcp::answer(&p, &snapshot, &|m| ipam.guest_of(m), &in_use, now);
        let host = match &change {
            dhcp::Change::Reserve(r) => r.name.clone(),
            _ => snapshot
                .reservation_for_mac(&p.mac())
                .and_then(|r| r.name.clone()),
        };
        let saved = match &change {
            dhcp::Change::None => Ok(()),
            dhcp::Change::Reserve(r) => {
                let r = r.clone();
                ipam.update(&net, |n| {
                    // Re-check under the lock: another guest may have taken
                    // it between the snapshot and now.
                    if n.reservation_for_ip(r.ip).is_some()
                        || n.reservation_for_mac(&r.mac).is_some()
                    {
                        return Err("raced".into());
                    }
                    n.reservations.push(r);
                    Ok(())
                })
            }
            dhcp::Change::Seen { mac, name } => ipam.update(&net, |n| {
                if let Some(r) = n.reservations.iter_mut().find(|r| &r.mac == mac) {
                    r.seen_ms = now;
                    if r.name.is_none() {
                        r.name = name.clone();
                    }
                }
                Ok(())
            }),
        };
        if let Err(e) = saved {
            // Do not answer with an address that was not recorded: the guest
            // retries and the next attempt sees the real state.
            tracing::warn!("ipam dhcp {net}: {e}");
            continue;
        }
        let Some(reply) = reply else { continue };
        if let dhcp::Change::Reserve(r) = &change {
            tracing::info!(
                "ipam: {net}: reserved {} for {} ({})",
                r.ip,
                r.mac,
                r.name.as_deref().unwrap_or("-")
            );
        }
        let bytes = dhcp::encode(&p, &reply, &snapshot, host.as_deref());
        let dest = dhcp::destination(&p, &reply);
        if let Err(e) = sock.send_to(&bytes, dest).await {
            tracing::warn!("ipam dhcp {net}: send to {dest:?}: {e}");
        }
    }
}

// ------------------------------------------------------------ DNS

async fn dns_udp_loop(ipam: Ipam, net: String, sock: UdpSocket) {
    let sock = Arc::new(sock);
    let mut buf = vec![0u8; 4096];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        let query = buf[..n].to_vec();
        let (ipam, net, sock) = (ipam.clone(), net.clone(), sock.clone());
        // Forwarding waits on the network; one slow upstream must not hold
        // up every other guest's lookups.
        tokio::spawn(async move {
            let client = match from.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                std::net::IpAddr::V6(_) => None,
            };
            if let Some(resp) = resolve(&ipam, &net, &query, false, client).await {
                let _ = sock.send_to(&resp, from).await;
            }
        });
    }
}

async fn dns_tcp_loop(ipam: Ipam, net: String, listener: tokio::net::TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut s, peer)) = listener.accept().await else {
            continue;
        };
        let (ipam, net) = (ipam.clone(), net.clone());
        tokio::spawn(async move {
            let t = std::time::Duration::from_secs(10);
            let mut len = [0u8; 2];
            while let Ok(Ok(_)) = tokio::time::timeout(t, s.read_exact(&mut len)).await {
                let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
                if tokio::time::timeout(t, s.read_exact(&mut q)).await.is_err() {
                    return;
                }
                let client = match peer.ip() {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    std::net::IpAddr::V6(_) => None,
                };
                let Some(resp) = resolve(&ipam, &net, &q, true, client).await else {
                    return;
                };
                let mut out = (resp.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&resp);
                if s.write_all(&out).await.is_err() {
                    return;
                }
            }
        });
    }
}

async fn resolve(
    ipam: &Ipam,
    net: &str,
    query: &[u8],
    tcp: bool,
    client: Option<Ipv4Addr>,
) -> Option<Vec<u8>> {
    let resp = answer_query(ipam, net, query, tcp).await?;
    // The access log: which name this guest asked for, and what it resolved
    // to, so its connections that follow can be shown by name. Only the
    // question and the A records are kept, never the full packet.
    // AAAA asks go unlogged: guests send one beside every A lookup, and on
    // these IPv4-only networks it would double every name with an empty row.
    if let (Some(c), Some(q)) = (client, dns::question(query).filter(|q| q.qtype != 28)) {
        let lookup = super::flows::Lookup {
            client: c,
            name: q.name,
            answers: dns::a_records(&resp),
        };
        ipam.flows.record_lookup(&lookup, ipam.owner(c), now_ms());
    }
    Some(resp)
}

async fn answer_query(ipam: &Ipam, net: &str, query: &[u8], tcp: bool) -> Option<Vec<u8>> {
    let snapshot = ipam.lock().networks.get(net).cloned()?;
    if let Some(q) = dns::question(query) {
        match dns::local_answer(&q, &snapshot) {
            dns::Local::Forward => {}
            local => return Some(dns::respond(query, &q, &local)),
        }
    }
    for up in dns::upstreams(&snapshot) {
        let r = if tcp {
            forward_tcp(up, query).await
        } else {
            forward_udp(up, query).await
        };
        if let Some(r) = r {
            return Some(r);
        }
    }
    dns::servfail(query)
}

async fn forward_udp(up: SocketAddr, query: &[u8]) -> Option<Vec<u8>> {
    let s = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    s.connect(up).await.ok()?;
    s.send(query).await.ok()?;
    let mut buf = vec![0u8; 4096];
    let t = std::time::Duration::from_millis(FORWARD_TIMEOUT_MS);
    loop {
        let n = tokio::time::timeout(t, s.recv(&mut buf)).await.ok()?.ok()?;
        // Only the answer to this query: same id.
        if n >= 2 && buf[..2] == query[..2] {
            return Some(buf[..n].to_vec());
        }
    }
}

async fn forward_tcp(up: SocketAddr, query: &[u8]) -> Option<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let t = std::time::Duration::from_millis(FORWARD_TIMEOUT_MS);
    let mut s = tokio::time::timeout(t, tokio::net::TcpStream::connect(up))
        .await
        .ok()?
        .ok()?;
    let mut out = (query.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(query);
    s.write_all(&out).await.ok()?;
    let mut len = [0u8; 2];
    tokio::time::timeout(t, s.read_exact(&mut len))
        .await
        .ok()?
        .ok()?;
    let mut r = vec![0u8; u16::from_be_bytes(len) as usize];
    tokio::time::timeout(t, s.read_exact(&mut r))
        .await
        .ok()?
        .ok()?;
    Some(r)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn guest_macs_are_read_from_every_interface() {
        let x = "<domain><devices>
          <interface type='network'><mac address='52:54:00:AA:00:01'/><source network='a'/></interface>
          <interface type='bridge'><source bridge='br0'/><mac address=\"52:54:00:aa:00:02\"/></interface>
          <interface type='network'><source network='a'/></interface>
        </devices></domain>";
        assert_eq!(
            super::macs(x),
            vec!["52:54:00:aa:00:01", "52:54:00:aa:00:02"]
        );
    }
}
