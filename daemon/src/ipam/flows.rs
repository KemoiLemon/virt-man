//! What guests connect to: one record per connection, from the kernel's
//! connection tracker, joined with the names they looked up through our DNS.
//! Both directions: the connections a guest opened, and the ones that reach it
//! (a port forward, the host itself), told apart by `dir`.
//!
//! Cheap by construction. NAT already makes the kernel track every guest
//! connection; subscribing to its NEW/DESTROY events costs one netlink message
//! per connection, not per packet, so a 10 GB download is one record. Byte
//! counts come from conntrack accounting (`nf_conntrack_acct`), a counter the
//! kernel bumps on an entry it is touching anyway. Nothing here inspects
//! packets.
//!
//! Records are kept in a bounded in-memory ring and pulled by the web server
//! (`GET /flows?after=<seq>`), which stores them. The daemon keeps no history:
//! if nobody pulls, the oldest records are dropped, never memory growth.
//!
//! Limits, by design: traffic between two guests on the same bridge is never
//! routed by the host, so conntrack does not see it; bridge-mode networks put
//! guests straight on the LAN, likewise. A guest using its own encrypted DNS
//! shows IPs without names.

use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Records kept for the web server to pull. At ~150 bytes each this is a few
/// MiB, and at the busiest measured rate still minutes of slack.
const RING: usize = 50_000;
/// How long a looked-up name stays attached to the addresses it resolved to.
/// Answers carry TTLs, but many are seconds long and the connection follows
/// the lookup by far longer than that on a slow guest; a fixed hour is the
/// useful reading of "which name was this IP".
pub(crate) const NAME_TTL_MS: u64 = 3_600_000;

// netlink / nfnetlink constants
const NETLINK_NETFILTER: i32 = 12;
const NFNLGRP_CONNTRACK_NEW: u32 = 1;
const NFNLGRP_CONNTRACK_DESTROY: u32 = 3;
const NFNL_SUBSYS_CTNETLINK: u16 = 1;
const IPCTNL_MSG_CT_NEW: u16 = 0;
const IPCTNL_MSG_CT_DELETE: u16 = 2;
const CTA_TUPLE_ORIG: u16 = 1;
const CTA_TUPLE_REPLY: u16 = 2;
const CTA_COUNTERS_ORIG: u16 = 9;
const CTA_COUNTERS_REPLY: u16 = 10;
const CTA_ID: u16 = 12;
const CTA_TUPLE_IP: u16 = 1;
const CTA_TUPLE_PROTO: u16 = 2;
const CTA_IP_V4_SRC: u16 = 1;
const CTA_IP_V4_DST: u16 = 2;
const CTA_PROTO_NUM: u16 = 1;
const CTA_PROTO_SRC_PORT: u16 = 2;
const CTA_PROTO_DST_PORT: u16 = 3;
const CTA_PROTO_ICMP_TYPE: u16 = 5;
const CTA_COUNTERS_BYTES: u16 = 2;
const CTA_COUNTERS_PACKETS: u16 = 1;

/// One connection event, as parsed from the kernel.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub destroy: bool,
    pub id: u32,
    pub proto: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub sport: u16,
    pub dport: u16,
    /// Source of the reply tuple. For an outbound connection that is the
    /// remote end again; for one that arrives, DNAT has rewritten the
    /// destination into a guest, so the guest appears here instead.
    pub reply_src: Option<Ipv4Addr>,
    /// The guest's own port on an inbound connection (the service behind a
    /// port forward), 0 when the reply tuple has none.
    pub reply_sport: u16,
    /// Only on destroy, and only with accounting on.
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub packets_out: u64,
    pub packets_in: u64,
}

/// Netlink attributes: (type without flags, payload) in order.
fn attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_ne_bytes([b[2], b[3]]) & 0x3fff;
        if len < 4 || len > b.len() {
            break;
        }
        out.push((ty, &b[4..len]));
        let step = (len + 3) & !3;
        if step >= b.len() {
            break;
        }
        b = &b[step..];
    }
    out
}

fn get<'a>(a: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    a.iter().find(|(t, _)| *t == ty).map(|(_, v)| *v)
}

fn be16(v: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(v.get(..2)?.try_into().ok()?))
}
fn be32(v: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(v.get(..4)?.try_into().ok()?))
}
fn be64(v: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(v.get(..8)?.try_into().ok()?))
}

/// (proto, src, dst, sport, dport) of a CTA_TUPLE_*.
fn tuple(b: &[u8]) -> Option<(u8, Ipv4Addr, Ipv4Addr, u16, u16)> {
    let a = attrs(b);
    let ip = attrs(get(&a, CTA_TUPLE_IP)?);
    let src = Ipv4Addr::from(be32(get(&ip, CTA_IP_V4_SRC)?)?);
    let dst = Ipv4Addr::from(be32(get(&ip, CTA_IP_V4_DST)?)?);
    let p = attrs(get(&a, CTA_TUPLE_PROTO)?);
    let proto = *get(&p, CTA_PROTO_NUM)?.first()?;
    let sport = get(&p, CTA_PROTO_SRC_PORT).and_then(be16).unwrap_or(0);
    // ICMP has no ports; its type is the useful "port" (8 = echo).
    let dport = get(&p, CTA_PROTO_DST_PORT)
        .and_then(be16)
        .or_else(|| get(&p, CTA_PROTO_ICMP_TYPE).and_then(|v| v.first().map(|&t| u16::from(t))))
        .unwrap_or(0);
    Some((proto, src, dst, sport, dport))
}

/// Source address and port of one tuple, leniently: only the address is
/// required. An event missing a reply port is still worth recording, so
/// nothing here may fail the whole event.
fn tuple_src(b: &[u8]) -> (Option<Ipv4Addr>, u16) {
    let a = attrs(b);
    let ip = attrs(get(&a, CTA_TUPLE_IP).unwrap_or(&[]));
    let p = attrs(get(&a, CTA_TUPLE_PROTO).unwrap_or(&[]));
    (
        get(&ip, CTA_IP_V4_SRC).and_then(be32).map(Ipv4Addr::from),
        get(&p, CTA_PROTO_SRC_PORT).and_then(be16).unwrap_or(0),
    )
}

fn counters(b: Option<&[u8]>) -> (u64, u64) {
    let Some(b) = b else { return (0, 0) };
    let a = attrs(b);
    (
        get(&a, CTA_COUNTERS_BYTES).and_then(be64).unwrap_or(0),
        get(&a, CTA_COUNTERS_PACKETS).and_then(be64).unwrap_or(0),
    )
}

/// Every conntrack event in one netlink datagram. IPv6 and anything else not
/// understood is skipped, not an error: the stream must never stop.
pub fn parse(mut d: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    while d.len() >= 16 {
        let len = u32::from_ne_bytes([d[0], d[1], d[2], d[3]]) as usize;
        let ty = u16::from_ne_bytes([d[4], d[5]]);
        if len < 16 || len > d.len() {
            break;
        }
        let msg = &d[..len];
        if ty >> 8 == NFNL_SUBSYS_CTNETLINK
            && matches!(ty & 0xff, IPCTNL_MSG_CT_NEW | IPCTNL_MSG_CT_DELETE)
            && msg.len() > 20
            // nfgenmsg family: AF_INET only.
            && msg[16] == 2
            && let Some(e) = event(&msg[20..], ty & 0xff == IPCTNL_MSG_CT_DELETE)
        {
            out.push(e);
        }
        let step = (len + 3) & !3;
        if step >= d.len() {
            break;
        }
        d = &d[step..];
    }
    out
}

fn event(body: &[u8], destroy: bool) -> Option<Event> {
    let a = attrs(body);
    let (proto, src, dst, sport, dport) = tuple(get(&a, CTA_TUPLE_ORIG)?)?;
    // The reply tuple is where the translated address shows: its source is a
    // guest when the connection came *in*, and its presence is what makes
    // this a complete entry.
    let (reply_src, reply_sport) = tuple_src(get(&a, CTA_TUPLE_REPLY)?);
    let (bytes_out, packets_out) = counters(get(&a, CTA_COUNTERS_ORIG));
    let (bytes_in, packets_in) = counters(get(&a, CTA_COUNTERS_REPLY));
    Some(Event {
        destroy,
        id: get(&a, CTA_ID).and_then(be32).unwrap_or(0),
        proto,
        src,
        dst,
        sport,
        dport,
        reply_src,
        reply_sport,
        bytes_out,
        bytes_in,
        packets_out,
        packets_in,
    })
}

// ------------------------------------------------------------ records

/// One connection, as served to the web server. Sent twice: when it opens
/// (`closed_ms` 0, no byte counts) and when it closes, with the same `ct_id`
/// and `opened_ms`, so the web server updates the row it already has. A long
/// connection (an SSH session, a download) is in the log from its start
/// rather than only days later when the kernel finally expires it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Flow {
    pub seq: u64,
    /// The kernel's id for the connection; with `opened_ms`, what makes the
    /// open and close records the same row. 0 for a DNS lookup.
    pub ct_id: u32,
    pub opened_ms: u64,
    pub closed_ms: u64,
    pub network: String,
    pub mac: Option<String>,
    pub vm: Option<String>,
    pub vm_name: Option<String>,
    /// "out": the guest opened the connection. "in": someone reached the
    /// guest. Either way `src`/`sport` are the guest's end and `dst`/`dport`
    /// the remote one, so a reader never has to check the direction to know
    /// which address the guest is.
    pub dir: String,
    pub proto: u8,
    pub src: Ipv4Addr,
    pub sport: u16,
    pub dst: Ipv4Addr,
    pub dport: u16,
    /// The name the guest looked up to reach `dst`, if it asked our DNS.
    pub domain: Option<String>,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub packets_out: u64,
    pub packets_in: u64,
}

/// A DNS query a guest made, recorded as its own row so a lookup with no
/// connection after it (blocked, cached, failed) is still visible.
///
/// dnsmasq answers DNS now, so nothing in-process fills this: the supervisor
/// reads its query log (`ipam::dnslog`) and feeds the answers here.
#[derive(Debug, Clone, PartialEq)]
pub struct Lookup {
    pub client: Ipv4Addr,
    pub name: String,
    pub answers: Vec<Ipv4Addr>,
}

#[derive(Default)]
struct Inner {
    seq: u64,
    ring: VecDeque<Flow>,
    /// conntrack id -> when it opened. Entries whose DESTROY never arrives
    /// (dropped event) are aged out with the name cache.
    open: HashMap<u32, u64>,
    /// (client, address) -> (name, when it was resolved)
    names: HashMap<(Ipv4Addr, Ipv4Addr), (String, u64)>,
    dropped: u64,
}

#[derive(Clone, Default)]
pub struct Flows(Arc<Mutex<Inner>>);

/// Who a guest address belongs to, from the IPAM store.
pub struct Owner {
    pub network: String,
    /// The network's gateway: our resolver, whose own lookups are logged by
    /// the DNS server with their names rather than as bare connections.
    pub gateway: Ipv4Addr,
    pub mac: String,
    pub vm: Option<String>,
    pub vm_name: Option<String>,
}

impl Flows {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().expect("flows poisoned")
    }

    /// Remember the addresses a guest's lookup resolved to, and record the
    /// lookup itself as a DNS flow row.
    pub fn record_lookup(&self, l: &Lookup, owner: Option<Owner>, now: u64) {
        let mut s = self.lock();
        for a in &l.answers {
            s.names.insert((l.client, *a), (l.name.clone(), now));
        }
        if let Some(o) = owner {
            push(&mut s, dns_flow(l, o, now));
        }
    }

    /// Name one more address a reply carried, without a row: a lookup with
    /// several A records is still one lookup, but a connection may use any of
    /// them.
    pub fn record_name(&self, client: Ipv4Addr, answer: Ipv4Addr, name: &str, now: u64) {
        self.lock()
            .names
            .insert((client, answer), (name.to_string(), now));
    }

    /// Fold one conntrack event in. Events with no guest at either end are
    /// ignored.
    pub fn record_event(&self, e: &Event, owner: impl Fn(Ipv4Addr) -> Option<Owner>, now: u64) {
        // A guest at the original source opened the connection. Otherwise it
        // can still be the destination -- the host, or a port forward whose
        // DNAT put the guest in the reply tuple's source. Everything else is
        // other people's traffic and not ours to log.
        let (o, dir) = match owner(e.src) {
            Some(o) => (o, "out"),
            None => match e.reply_src.and_then(&owner) {
                Some(o) => (o, "in"),
                None => return,
            },
        };
        let inbound = dir == "in";
        let mut s = self.lock();
        let opened = if e.destroy {
            s.open.remove(&e.id).unwrap_or(now)
        } else {
            s.open.insert(e.id, now);
            now
        };
        // DNS to our own resolver is logged by the resolver, with the name.
        // Only the guest's own lookups: a connection *arriving* on port 53 is
        // someone using the guest as a server, not a lookup.
        if !inbound && e.dport == 53 && e.dst == o.gateway {
            return;
        }
        // The row reads guest -> remote in both directions, so an inbound
        // connection has its tuple the other way round: the remote end is the
        // one that dialled, and the guest's port is the forwarded-to one.
        // Careful with the ports -- in the inbound tuple `e.dport` is the
        // *guest's* port (the WAN one, before DNAT), so the remote's port is
        // `e.sport`, and the guest's is the reply tuple's.
        let (src, sport, dst, dport) = if inbound {
            (
                e.reply_src.unwrap_or(Ipv4Addr::UNSPECIFIED),
                if e.reply_sport != 0 {
                    e.reply_sport
                } else {
                    e.dport
                },
                e.src,
                e.sport,
            )
        } else {
            (e.src, e.sport, e.dst, e.dport)
        };
        let domain = s
            .names
            .get(&(src, dst))
            .filter(|(_, at)| now.saturating_sub(*at) < NAME_TTL_MS)
            .map(|(n, _)| n.clone());
        // The kernel counts per conntrack direction, not per guest: on an
        // inbound connection the reply direction is the one the guest sent.
        let (bytes_out, packets_out, bytes_in, packets_in) = if inbound {
            (e.bytes_in, e.packets_in, e.bytes_out, e.packets_out)
        } else {
            (e.bytes_out, e.packets_out, e.bytes_in, e.packets_in)
        };
        let f = Flow {
            seq: 0,
            ct_id: e.id,
            opened_ms: opened,
            closed_ms: if e.destroy { now } else { 0 },
            network: o.network,
            mac: Some(o.mac),
            vm: o.vm,
            vm_name: o.vm_name,
            dir: dir.into(),
            proto: e.proto,
            src,
            sport,
            dst,
            dport,
            domain,
            bytes_out,
            bytes_in,
            packets_out,
            packets_in,
        };
        push(&mut s, f);
    }

    /// Records after `seq`, oldest first, at most `limit`; and the sequence of
    /// the newest record, so a caller that fell behind the ring can tell.
    pub fn since(&self, after: u64, limit: usize) -> (Vec<Flow>, u64, u64) {
        let s = self.lock();
        let out = s
            .ring
            .iter()
            .filter(|f| f.seq > after)
            .take(limit)
            .cloned()
            .collect();
        (out, s.seq, s.dropped)
    }

    /// Drop stale name mappings and open entries that never closed.
    pub fn sweep(&self, now: u64) {
        let mut s = self.lock();
        s.names
            .retain(|_, (_, at)| now.saturating_sub(*at) < NAME_TTL_MS);
        s.open
            .retain(|_, at| now.saturating_sub(*at) < 24 * NAME_TTL_MS);
    }
}

fn push(s: &mut Inner, mut f: Flow) {
    s.seq += 1;
    f.seq = s.seq;
    if s.ring.len() >= RING {
        s.ring.pop_front();
        s.dropped += 1;
    }
    s.ring.push_back(f);
}

fn dns_flow(l: &Lookup, o: Owner, now: u64) -> Flow {
    Flow {
        seq: 0,
        ct_id: 0,
        opened_ms: now,
        closed_ms: now,
        network: o.network,
        mac: Some(o.mac),
        vm: o.vm,
        vm_name: o.vm_name,
        dir: "out".into(),
        proto: 17,
        src: l.client,
        sport: 0,
        dst: l.answers.first().copied().unwrap_or(Ipv4Addr::UNSPECIFIED),
        dport: 53,
        domain: Some(l.name.clone()),
        bytes_out: 0,
        bytes_in: 0,
        packets_out: 0,
        packets_in: 0,
    }
}

// ------------------------------------------------------------ the listener

/// Turn on per-connection byte counters. Without them every record says 0
/// bytes; with them, the kernel adds to a counter on an entry it is already
/// updating for NAT.
pub fn enable_accounting() {
    if let Err(e) = std::fs::write("/proc/sys/net/netfilter/nf_conntrack_acct", "1") {
        tracing::warn!("flows: cannot enable conntrack accounting ({e}); byte counts will be 0");
    }
}

/// Subscribe to conntrack NEW and DESTROY events and fold them in forever.
/// Runs on a dedicated thread: a blocking netlink read, nothing else.
pub fn listen(flows: Flows, owner: impl Fn(Ipv4Addr) -> Option<Owner> + Send + 'static) {
    std::thread::Builder::new()
        .name("flows".into())
        .spawn(move || {
            loop {
                if let Err(e) = run(&flows, &owner) {
                    tracing::warn!("flows: {e}; retrying in 5 s");
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("flows: cannot start listener: {e}"));
}

fn run(flows: &Flows, owner: &dyn Fn(Ipv4Addr) -> Option<Owner>) -> Result<(), String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(
        Domain::from(libc::AF_NETLINK),
        Type::RAW,
        Some(Protocol::from(NETLINK_NETFILTER)),
    )
    .map_err(|e| format!("netlink socket: {e}"))?;
    // A large receive buffer: a burst of short connections (a guest's
    // package manager opening hundreds) must not overflow it.
    let _ = sock.set_recv_buffer_size(8 << 20);
    let groups = (1u32 << (NFNLGRP_CONNTRACK_NEW - 1)) | (1u32 << (NFNLGRP_CONNTRACK_DESTROY - 1));
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    addr.nl_groups = groups;
    let rc = unsafe {
        libc::bind(
            std::os::fd::AsRawFd::as_raw_fd(&sock),
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if rc != 0 {
        return Err(format!(
            "bind conntrack groups: {}",
            std::io::Error::last_os_error()
        ));
    }
    tracing::info!("flows: listening for guest connections");
    let mut buf = vec![0u8; 256 * 1024];
    let mut last_sweep = now_ms();
    loop {
        let n = unsafe {
            libc::recv(
                std::os::fd::AsRawFd::as_raw_fd(&sock),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            // ENOBUFS: the kernel dropped events because we fell behind. Say
            // so and carry on; stopping would lose everything after it.
            if e.raw_os_error() == Some(libc::ENOBUFS) {
                tracing::warn!("flows: kernel dropped connection events (burst too large)");
                continue;
            }
            return Err(format!("recv: {e}"));
        }
        let now = now_ms();
        for ev in parse(&buf[..n as usize]) {
            flows.record_event(&ev, owner, now);
        }
        if now.saturating_sub(last_sweep) > 60_000 {
            flows.sweep(now);
            last_sweep = now;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Captured from this host's kernel (7.2): a NEW for a TCP connection to
    /// port 80, and a DESTROY for a UDP lookup, with accounting on.
    const NEW_TCP: &str = "c400000000010006000000000000000002000000340001801400018008000100c0a8007708000200ac429ded1c000280050001000600000006000200de6e00000600030000500000340002801400018008000100ac429ded08000200c0a800771c0002800500010006000000060002000050000006000300de6e000008000c00ce26f9c208000300000001880800070000000078300004802c0001800500010001000000050002000a000000050003000000000006000400030000000600050000000000";
    /// Captured from this host's kernel: the host opening a connection *to*
    /// a guest on a managed network. The guest is the original destination,
    /// and -- the fact this test exists for -- the reply tuple's source, which
    /// is what tells the daemon the connection came in rather than went out.
    /// Note the ports: the tuple's `dport` is the guest's port, so reading the
    /// incoming tuple the obvious way puts the guest's port on both ends.
    const NEW_INBOUND: &str = "c400000000010006000000000000000002000000340001801400018008000100c0a8a70108000200c0a8a7021c0002800500010006000000060002009e98000006000300270f0000340002801400018008000100c0a8a70208000200c0a8a7011c000280050001000600000006000200270f0000060003009e98000008000c00ce12ec5d08000300000001880800070000000078300004802c0001800500010001000000050002000a000000050003000000000006000400030000000600050000000000";

    const DESTROY_UDP: &str = "c400000002010000000000000000000002000000340001801400018008000100c0a8007708000200c0a8000b1c000280050001001100000006000200df0600000600030000350000340002801400018008000100c0a8000b08000200c0a800771c0002800500010011000000060002000035000006000300df06000008000c00717af7a8080003000000038a1c0009800c00010000000000000000010c00020000000000000000501c000a800c00010000000000000000010c0002000000000000000070";

    #[test]
    fn a_real_new_event_is_parsed() {
        let e = parse(&hex(NEW_TCP));
        assert_eq!(e.len(), 1);
        let e = &e[0];
        assert!(!e.destroy);
        assert_eq!((e.proto, e.dport), (6, 80));
        assert_eq!(e.src, "192.168.0.119".parse::<Ipv4Addr>().unwrap());
        assert_eq!(e.dst, "172.66.157.237".parse::<Ipv4Addr>().unwrap());
        assert_eq!(e.sport, 0xde6e);
        assert_ne!(e.id, 0);
        // The reply tuple of an outbound connection is the remote answering:
        // no guest there, so this one is an "out" row.
        assert_eq!(
            (e.reply_src.unwrap().to_string(), e.reply_sport),
            ("172.66.157.237".to_string(), 80)
        );
    }

    #[test]
    fn a_real_destroy_event_carries_the_byte_counts() {
        let e = &parse(&hex(DESTROY_UDP))[0];
        assert!(e.destroy);
        assert_eq!((e.proto, e.dport), (17, 53));
        assert_eq!((e.packets_out, e.bytes_out), (1, 80));
        assert_eq!((e.packets_in, e.bytes_in), (1, 112));
    }

    #[test]
    fn several_messages_in_one_datagram_and_junk_are_handled() {
        let mut two = hex(NEW_TCP);
        two.extend(hex(DESTROY_UDP));
        assert_eq!(parse(&two).len(), 2);
        assert!(parse(&[]).is_empty());
        assert!(
            parse(&[0xff; 40]).is_empty(),
            "a length larger than the buffer"
        );
        let mut cut = hex(NEW_TCP);
        cut.truncate(90);
        cut[0] = 90;
        assert!(
            parse(&cut).is_empty(),
            "a truncated message yields nothing, not a panic"
        );
    }

    fn owner(ip: Ipv4Addr) -> Option<Owner> {
        (ip.octets()[..3] == [10, 9, 0] && ip.octets()[3] != 1).then(|| Owner {
            network: "lab".into(),
            gateway: "10.9.0.1".parse().unwrap(),
            mac: "52:54:00:00:00:01".into(),
            vm: Some("uuid".into()),
            vm_name: Some("web".into()),
        })
    }

    fn ev(destroy: bool, id: u32, src: &str, dst: &str, dport: u16) -> Event {
        Event {
            destroy,
            id,
            proto: 6,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            sport: 40000,
            dport,
            // What the kernel reports for an outbound connection: the reply
            // tuple is the remote end answering.
            reply_src: Some(dst.parse().unwrap()),
            reply_sport: dport,
            bytes_out: 100,
            bytes_in: 5000,
            packets_out: 2,
            packets_in: 5,
        }
    }

    #[test]
    fn a_connection_is_recorded_once_with_its_open_time_and_the_name_looked_up() {
        let f = Flows::default();
        f.record_lookup(
            &Lookup {
                client: "10.9.0.7".parse().unwrap(),
                name: "github.com".into(),
                answers: vec!["140.82.112.3".parse().unwrap()],
            },
            owner("10.9.0.7".parse().unwrap()),
            1_000,
        );
        f.record_event(&ev(false, 7, "10.9.0.7", "140.82.112.3", 443), owner, 2_000);
        let open = f.since(1, 100).0;
        assert_eq!(open.len(), 1, "an open connection is reported at once");
        assert_eq!(
            (open[0].opened_ms, open[0].closed_ms, open[0].ct_id),
            (2_000, 0, 7)
        );
        f.record_event(&ev(true, 7, "10.9.0.7", "140.82.112.3", 443), owner, 9_000);
        let (all, last, _) = f.since(0, 100);
        assert_eq!(last, 3);
        let conn = &all[2];
        assert_eq!(
            (conn.opened_ms, conn.closed_ms, conn.ct_id),
            (2_000, 9_000, 7),
            "the close names the same row"
        );
        assert_eq!(conn.domain.as_deref(), Some("github.com"));
        assert_eq!((conn.bytes_out, conn.bytes_in), (100, 5000));
        assert_eq!(conn.vm_name.as_deref(), Some("web"));
        let dns = &all[0];
        assert_eq!((dns.dport, dns.domain.as_deref()), (53, Some("github.com")));
        assert_eq!(
            f.since(2, 100).0.len(),
            1,
            "a cursor skips what was already pulled"
        );
    }

    #[test]
    fn a_second_address_of_one_lookup_names_a_connection_without_a_second_row() {
        let f = Flows::default();
        f.record_lookup(
            &Lookup {
                client: "10.9.0.7".parse().unwrap(),
                name: "example.com".into(),
                answers: vec!["1.2.3.4".parse().unwrap()],
            },
            owner("10.9.0.7".parse().unwrap()),
            1_000,
        );
        f.record_name(
            "10.9.0.7".parse().unwrap(),
            "5.6.7.8".parse().unwrap(),
            "example.com",
            1_000,
        );
        let rows = f.since(0, 100).0;
        assert_eq!(rows.len(), 1, "one lookup is one DNS row");
        f.record_event(&ev(false, 8, "10.9.0.7", "5.6.7.8", 443), owner, 2_000);
        let rows = f.since(0, 100).0;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].domain.as_deref(), Some("example.com"));
    }

    #[test]
    fn an_inbound_connection_reads_guest_first_like_an_outbound_one() {
        let f = Flows::default();
        // A port forward: the stranger dials the host's WAN address, and the
        // reply tuple carries the guest DNAT rewrote it to. The remote's port
        // is the one it dialled from; the guest's is the reply tuple's.
        let mut e = ev(false, 9, "203.0.113.5", "192.0.2.1", 2222);
        e.sport = 51_000;
        e.reply_src = Some("10.9.0.7".parse().unwrap());
        e.reply_sport = 22;
        f.record_event(&e, owner, 5_000);
        let (all, _, _) = f.since(0, 10);
        let row = &all[0];
        assert_eq!(row.dir, "in");
        assert_eq!(row.vm_name.as_deref(), Some("web"));
        assert_eq!(
            (row.src.to_string(), row.sport),
            ("10.9.0.7".into(), 22),
            "the guest end is the source, with its own port"
        );
        assert_eq!(
            (row.dst.to_string(), row.dport),
            ("203.0.113.5".into(), 51_000),
            "…and the remote end the destination, on the port it dialled from"
        );
        // The kernel counts per conntrack direction: what the guest sent is
        // the reply direction here, so the counters cannot be copied across.
        assert_eq!((row.bytes_out, row.bytes_in), (5000, 100));
        // A connection arriving on 53 is somebody using the guest as a
        // resolver, not the guest's own lookup.
        let mut dns = ev(false, 10, "203.0.113.9", "10.9.0.1", 53);
        dns.reply_src = Some("10.9.0.7".parse().unwrap());
        f.record_event(&dns, owner, 6_000);
        assert_eq!(f.since(0, 10).0.len(), 2);
    }

    #[test]
    fn a_real_inbound_event_is_parsed_as_arriving() {
        let e = &parse(&hex(NEW_INBOUND))[0];
        assert!(!e.destroy);
        assert_eq!((e.proto, e.sport, e.dport), (6, 40600, 9999));
        assert_eq!(
            (e.src.to_string(), e.dst.to_string()),
            ("192.168.167.1".to_string(), "192.168.167.2".to_string()),
            "the host dialled the guest"
        );
        // The guest is the reply tuple's source, which is what marks it inbound.
        assert_eq!(e.reply_src.unwrap().to_string(), "192.168.167.2");
        assert_eq!(e.reply_sport, 9999);

        // …and folding it in reads guest-first, with the counters the right
        // way round, exactly as the outbound case does. Only the guest's own
        // address belongs to a guest here; the host dialling it does not.
        let f = Flows::default();
        let guest = |ip: Ipv4Addr| {
            (ip.to_string() == "192.168.167.2").then(|| Owner {
                network: "internet".into(),
                gateway: "192.168.167.1".parse().unwrap(),
                mac: "52:54:00:82:de:19".into(),
                vm: Some("uuid".into()),
                vm_name: Some("alpine".into()),
            })
        };
        f.record_event(e, guest, 42);
        let (all, _, _) = f.since(0, 10);
        let row = &all[0];
        assert_eq!(row.dir, "in");
        assert_eq!(
            (
                row.src.to_string(),
                row.sport,
                row.dst.to_string(),
                row.dport
            ),
            (
                "192.168.167.2".to_string(),
                9999,
                "192.168.167.1".to_string(),
                40600
            ),
            "guest first, on the port it was reached on"
        );
    }

    #[test]
    fn host_traffic_and_lookups_to_our_resolver_are_not_connection_rows() {
        let f = Flows::default();
        f.record_event(&ev(true, 1, "192.168.0.119", "1.1.1.1", 443), owner, 1);
        f.record_event(&ev(true, 2, "10.9.0.7", "10.9.0.1", 53), owner, 1);
        assert!(f.since(0, 10).0.is_empty());
    }

    #[test]
    fn a_name_goes_stale_and_the_ring_is_bounded() {
        let f = Flows::default();
        f.record_lookup(
            &Lookup {
                client: "10.9.0.7".parse().unwrap(),
                name: "old.example".into(),
                answers: vec!["1.2.3.4".parse().unwrap()],
            },
            None,
            0,
        );
        f.record_event(
            &ev(true, 3, "10.9.0.7", "1.2.3.4", 443),
            owner,
            NAME_TTL_MS + 1,
        );
        assert_eq!(f.since(0, 10).0[0].domain, None);
        for i in 0..(RING as u32 + 5) {
            f.record_event(&ev(true, 100 + i, "10.9.0.7", "1.2.3.4", 443), owner, 1);
        }
        let (all, last, dropped) = f.since(0, usize::MAX);
        assert_eq!(all.len(), RING);
        assert_eq!(dropped, 6);
        assert_eq!(all.last().unwrap().seq, last);
    }
}
