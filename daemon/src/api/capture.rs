//! Live packet capture on a guest's interfaces, only while someone watches.
//!
//! `POST /domains/{name}/capture` opens an `AF_PACKET` socket on each of the
//! guest's tap devices and returns a session id; `GET …/capture/{id}?after=N`
//! returns the packets decoded since `N`; `GET …/capture/{id}/pcap` returns the
//! captured bytes as a pcap file for Wireshark; `DELETE` stops it.
//!
//! Nothing is captured unless a session is open, so the host pays nothing
//! the rest of the time. A session stops by itself when nobody has polled it
//! for [`IDLE`], so a closed browser tab never leaves a capture running, and
//! after [`MAX_AGE`] whatever happens. Each session keeps at most [`RING`]
//! packets, each truncated to [`SNAPLEN`] bytes: memory is bounded however
//! busy the guest is.

use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use virt::domain::Domain;

use super::ApiError;
use crate::libvirt::with_conn;

pub const IDLE: Duration = Duration::from_secs(20);
pub const MAX_AGE: Duration = Duration::from_secs(30 * 60);
pub const RING: usize = 5000;
pub const SNAPLEN: usize = 1600;
const MAX_SESSIONS: usize = 8;

/// One captured packet, decoded for display.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Line {
    pub seq: u64,
    pub ts_us: u64,
    /// "out" = sent by the guest, "in" = delivered to it.
    pub dir: &'static str,
    pub iface: String,
    pub len: usize,
    pub proto: String,
    pub src: String,
    pub dst: String,
    pub sport: Option<u16>,
    pub dport: Option<u16>,
    /// TCP flags ("SYN", "ACK PSH"…), ICMP type, a DNS question, an ARP who-has.
    pub info: String,
}

struct Captured {
    line: Line,
    bytes: Vec<u8>,
    orig_len: usize,
}

struct Session {
    vm: String,
    started: Instant,
    last_poll: Mutex<Instant>,
    stop: AtomicBool,
    ring: Mutex<VecDeque<Captured>>,
    seq: Mutex<u64>,
    dropped: Mutex<u64>,
    filter: Filter,
}

/// Optional narrowing, applied before a packet is kept.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub proto: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub host: Option<Ipv4Addr>,
}

impl Filter {
    fn keeps(&self, l: &Line) -> bool {
        if let Some(p) = self
            .proto
            .as_deref()
            .filter(|p| !p.is_empty() && *p != "any")
            && !l.proto.eq_ignore_ascii_case(p)
        {
            return false;
        }
        if let Some(port) = self.port
            && l.sport != Some(port)
            && l.dport != Some(port)
        {
            return false;
        }
        if let Some(h) = self.host {
            let h = h.to_string();
            if l.src != h && l.dst != h {
                return false;
            }
        }
        true
    }
}

type Sessions = Mutex<HashMap<String, Arc<Session>>>;

fn sessions() -> &'static Sessions {
    static S: OnceLock<Sessions> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn routes() -> Router {
    Router::new()
        .route("/domains/{name}/capture", post(start))
        .route("/domains/{name}/capture/{id}", get(poll).delete(stop))
        .route("/domains/{name}/capture/{id}/pcap", get(pcap))
}

async fn start(
    Path(name): Path<String>,
    body: Option<Json<Filter>>,
) -> Result<Json<Value>, ApiError> {
    let filter = body.map(|Json(f)| f).unwrap_or_default();
    let n = name.clone();
    let taps = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &n)?;
        if !d.is_active().unwrap_or(false) {
            return Err("the guest is not running".to_string().into());
        }
        Ok(taps(&d.get_xml_desc(0)?))
    })
    .await
    .map_err(ApiError::bad_request)?;
    if taps.is_empty() {
        return Err(ApiError::bad_request(
            "the guest has no network interface to capture",
        ));
    }
    reap();
    if sessions().lock().expect("capture sessions poisoned").len() >= MAX_SESSIONS {
        return Err(ApiError::conflict(
            "too many captures running; stop one first",
        ));
    }
    let id = format!("{:016x}", rand_id());
    let s = Arc::new(Session {
        vm: name.clone(),
        started: Instant::now(),
        last_poll: Mutex::new(Instant::now()),
        stop: AtomicBool::new(false),
        ring: Mutex::new(VecDeque::new()),
        seq: Mutex::new(0),
        dropped: Mutex::new(0),
        filter,
    });
    for tap in &taps {
        let (s, tap) = (s.clone(), tap.clone());
        std::thread::Builder::new()
            .name(format!("capture-{tap}"))
            .spawn(move || {
                if let Err(e) = capture_loop(&s, &tap) {
                    tracing::warn!("capture on {tap}: {e}");
                }
            })
            .map_err(ApiError::internal)?;
    }
    sessions()
        .lock()
        .expect("capture sessions poisoned")
        .insert(id.clone(), s);
    tracing::info!("capture {id} started on {name} ({})", taps.join(", "));
    Ok(Json(
        json!({"id": id, "interfaces": taps, "idle_secs": IDLE.as_secs(), "ring": RING}),
    ))
}

fn session(name: &str, id: &str) -> Result<Arc<Session>, ApiError> {
    let s = sessions()
        .lock()
        .expect("capture sessions poisoned")
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("capture has ended"))?;
    if s.vm != name {
        return Err(ApiError::not_found("capture has ended"));
    }
    Ok(s)
}

#[derive(Deserialize)]
struct PollQuery {
    #[serde(default)]
    after: u64,
}

/// Packets since `after`. Polling keeps the session alive.
async fn poll(
    Path((name, id)): Path<(String, String)>,
    Query(q): Query<PollQuery>,
) -> Result<Json<Value>, ApiError> {
    let s = session(&name, &id)?;
    *s.last_poll.lock().expect("poll lock poisoned") = Instant::now();
    let ring = s.ring.lock().expect("ring poisoned");
    let lines: Vec<&Line> = ring
        .iter()
        .map(|c| &c.line)
        .filter(|l| l.seq > q.after)
        .take(1000)
        .collect();
    Ok(Json(json!({
        "lines": lines,
        "last": *s.seq.lock().expect("seq poisoned"),
        "dropped": *s.dropped.lock().expect("dropped poisoned"),
        "kept": ring.len(),
    })))
}

async fn stop(Path((name, id)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let s = session(&name, &id)?;
    s.stop.store(true, Ordering::Relaxed);
    sessions()
        .lock()
        .expect("capture sessions poisoned")
        .remove(&id);
    Ok(Json(json!({"id": id, "stopped": true})))
}

/// What the session holds, as a pcap file.
async fn pcap(Path((name, id)): Path<(String, String)>) -> Result<impl IntoResponse, ApiError> {
    let s = session(&name, &id)?;
    let ring = s.ring.lock().expect("ring poisoned");
    let body = write_pcap(
        ring.iter()
            .map(|c| (c.line.ts_us, c.orig_len, c.bytes.as_slice())),
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.tcpdump.pcap"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"capture.pcap\"",
            ),
        ],
        body,
    ))
}

/// Stop sessions nobody is watching, or that have run too long.
fn reap() {
    let mut map = sessions().lock().expect("capture sessions poisoned");
    map.retain(|id, s| {
        let idle = s
            .last_poll
            .lock()
            .map(|t| t.elapsed() > IDLE)
            .unwrap_or(true);
        let old = s.started.elapsed() > MAX_AGE;
        if idle || old {
            s.stop.store(true, Ordering::Relaxed);
            tracing::info!(
                "capture {id} on {} stopped ({})",
                s.vm,
                if old { "too old" } else { "nobody watching" }
            );
            false
        } else {
            true
        }
    });
}

/// Reap on a timer too, so a session nobody polls stops even if no new
/// capture is ever started.
pub async fn reaper() {
    let mut t = tokio::time::interval(Duration::from_secs(5));
    loop {
        t.tick().await;
        reap();
    }
}

// ------------------------------------------------------------ the socket

fn capture_loop(s: &Session, tap: &str) -> Result<(), String> {
    const ETH_P_ALL: u16 = 0x0003;
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW,
            i32::from(ETH_P_ALL.to_be()),
        )
    };
    if fd < 0 {
        return Err(format!("socket: {}", std::io::Error::last_os_error()));
    }
    let close = Fd(fd);
    let ifindex = unsafe {
        let c = std::ffi::CString::new(tap).map_err(|e| e.to_string())?;
        libc::if_nametoindex(c.as_ptr())
    };
    if ifindex == 0 {
        return Err(format!("no interface {tap}"));
    }
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = ETH_P_ALL.to_be();
    sll.sll_ifindex = ifindex as i32;
    let rc = unsafe {
        libc::bind(
            close.0,
            (&sll as *const libc::sockaddr_ll).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        )
    };
    if rc != 0 {
        return Err(format!("bind {tap}: {}", std::io::Error::last_os_error()));
    }
    // Wake up regularly to notice a stop, even when the guest is silent.
    let tv = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            close.0,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&tv as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as u32,
        );
    }
    let mut buf = vec![0u8; 65536];
    while !s.stop.load(Ordering::Relaxed) {
        let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut flen = std::mem::size_of::<libc::sockaddr_ll>() as u32;
        let n = unsafe {
            libc::recvfrom(
                close.0,
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_TRUNC,
                (&mut from as *mut libc::sockaddr_ll).cast(),
                &mut flen,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(format!("recv: {e}"));
        }
        let orig_len = n as usize;
        let got = &buf[..orig_len.min(buf.len())];
        // On a tap, what the host transmits is what the guest receives.
        let dir = if from.sll_pkttype == libc::PACKET_OUTGOING {
            "in"
        } else {
            "out"
        };
        let ts_us = now_us();
        let Some(mut line) = decode(got, dir, tap, ts_us) else {
            continue;
        };
        line.len = orig_len;
        if !s.filter.keeps(&line) {
            continue;
        }
        let mut seq = s.seq.lock().expect("seq poisoned");
        *seq += 1;
        line.seq = *seq;
        drop(seq);
        let mut ring = s.ring.lock().expect("ring poisoned");
        if ring.len() >= RING {
            ring.pop_front();
            *s.dropped.lock().expect("dropped poisoned") += 1;
        }
        ring.push_back(Captured {
            line,
            bytes: got[..got.len().min(SNAPLEN)].to_vec(),
            orig_len,
        });
    }
    Ok(())
}

struct Fd(i32);
impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

// ------------------------------------------------------------ decoding

/// A one-line summary of an Ethernet frame. None only for a frame too short
/// to have an Ethernet header.
pub fn decode(f: &[u8], dir: &'static str, iface: &str, ts_us: u64) -> Option<Line> {
    if f.len() < 14 {
        return None;
    }
    let mac = |o: usize| {
        f[o..o + 6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    let mut ethertype = u16::from_be_bytes([f[12], f[13]]);
    let mut off = 14;
    if ethertype == 0x8100 && f.len() >= 18 {
        ethertype = u16::from_be_bytes([f[16], f[17]]);
        off = 18;
    }
    let mut l = Line {
        seq: 0,
        ts_us,
        dir,
        iface: iface.to_string(),
        len: f.len(),
        proto: format!("0x{ethertype:04x}"),
        src: mac(6),
        dst: mac(0),
        sport: None,
        dport: None,
        info: String::new(),
    };
    let p = &f[off..];
    match ethertype {
        0x0800 => ipv4(p, &mut l),
        0x0806 => arp(p, &mut l),
        0x86dd => ipv6(p, &mut l),
        // Values up to 1500 are an 802.3 length, not a type: LLC follows.
        // On a guest tap that is the bridge's spanning-tree hello.
        len if len <= 1500 => {
            let stp = p.get(..3) == Some(&[0x42, 0x42, 0x03][..]);
            l.proto = if stp { "stp" } else { "llc" }.into();
            if stp {
                l.info = "spanning tree (bridge)".into();
            }
        }
        _ => {}
    }
    Some(l)
}

fn ipv4(p: &[u8], l: &mut Line) {
    if p.len() < 20 || p[0] >> 4 != 4 {
        l.proto = "ipv4?".into();
        return;
    }
    let ihl = usize::from(p[0] & 0x0f) * 4;
    let proto = p[9];
    l.src = Ipv4Addr::new(p[12], p[13], p[14], p[15]).to_string();
    l.dst = Ipv4Addr::new(p[16], p[17], p[18], p[19]).to_string();
    let frag = u16::from_be_bytes([p[6], p[7]]) & 0x1fff != 0;
    let t = p.get(ihl..).unwrap_or(&[]);
    transport(proto, t, frag, l);
}

fn ipv6(p: &[u8], l: &mut Line) {
    if p.len() < 40 {
        l.proto = "ipv6?".into();
        return;
    }
    let addr = |o: usize| {
        let mut a = [0u8; 16];
        a.copy_from_slice(&p[o..o + 16]);
        std::net::Ipv6Addr::from(a).to_string()
    };
    l.src = addr(8);
    l.dst = addr(24);
    transport(p[6], &p[40..], false, l);
    if l.proto == "icmp" {
        l.proto = "icmpv6".into();
    }
}

fn transport(proto: u8, t: &[u8], frag: bool, l: &mut Line) {
    let port = |o: usize| t.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    match proto {
        6 => {
            l.proto = "tcp".into();
            if frag {
                return l.info.push_str("fragment");
            }
            l.sport = port(0);
            l.dport = port(2);
            if let Some(&fl) = t.get(13) {
                let names = [
                    (0x02, "SYN"),
                    (0x10, "ACK"),
                    (0x08, "PSH"),
                    (0x01, "FIN"),
                    (0x04, "RST"),
                    (0x20, "URG"),
                ];
                l.info = names
                    .iter()
                    .filter(|(b, _)| fl & b != 0)
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }
        17 => {
            l.proto = "udp".into();
            if frag {
                return l.info.push_str("fragment");
            }
            l.sport = port(0);
            l.dport = port(2);
            let payload = t.get(8..).unwrap_or(&[]);
            if l.sport == Some(53) || l.dport == Some(53) {
                l.proto = "dns".into();
                l.info = dns_summary(payload);
            } else if matches!((l.sport, l.dport), (Some(67 | 68), Some(67 | 68))) {
                l.proto = "dhcp".into();
            }
        }
        1 | 58 => {
            l.proto = "icmp".into();
            if let Some(&ty) = t.first() {
                l.info = match (proto, ty) {
                    (1, 8) | (58, 128) => "echo request".into(),
                    (1, 0) | (58, 129) => "echo reply".into(),
                    (1, 3) => "unreachable".into(),
                    (1, 11) => "time exceeded".into(),
                    (58, 135) => "neighbor solicitation".into(),
                    (58, 136) => "neighbor advertisement".into(),
                    (58, 133) => "router solicitation".into(),
                    (58, 134) => "router advertisement".into(),
                    _ => format!("type {ty}"),
                };
            }
        }
        2 => l.proto = "igmp".into(),
        other => l.proto = format!("ip-{other}"),
    }
}

fn arp(p: &[u8], l: &mut Line) {
    l.proto = "arp".into();
    if p.len() < 28 {
        return;
    }
    let spa = Ipv4Addr::new(p[14], p[15], p[16], p[17]);
    let tpa = Ipv4Addr::new(p[24], p[25], p[26], p[27]);
    l.src = spa.to_string();
    l.dst = tpa.to_string();
    l.info = match u16::from_be_bytes([p[6], p[7]]) {
        1 => format!("who has {tpa}? tell {spa}"),
        2 => format!(
            "{spa} is at {}",
            p[8..14]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        ),
        op => format!("op {op}"),
    };
}

/// "A? github.com" for a query, "A github.com → 140.82.112.3" for an answer.
fn dns_summary(b: &[u8]) -> String {
    let Some(q) = crate::ipam::dns::question_any(b) else {
        return String::new();
    };
    let ty = match q.1 {
        1 => "A",
        28 => "AAAA",
        5 => "CNAME",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        33 => "SRV",
        65 => "HTTPS",
        t => return format!("type {t} {}", q.0),
    };
    if b.len() > 2 && b[2] & 0x80 != 0 {
        let ips = crate::ipam::dns::a_records(b);
        let ans: Vec<String> = ips.iter().map(ToString::to_string).collect();
        if ans.is_empty() {
            let rcode = b.get(3).map_or(0, |r| r & 0x0f);
            return format!(
                "{ty} {} → {}",
                q.0,
                if rcode == 3 {
                    "no such name"
                } else {
                    "no addresses"
                }
            );
        }
        return format!("{ty} {} → {}", q.0, ans.join(", "));
    }
    format!("{ty}? {}", q.0)
}

/// Captured frames as a classic pcap file (Ethernet link type).
pub fn write_pcap<'a>(frames: impl Iterator<Item = (u64, usize, &'a [u8])>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(SNAPLEN as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // LINKTYPE_ETHERNET
    for (ts, orig, bytes) in frames {
        out.extend_from_slice(&((ts / 1_000_000) as u32).to_le_bytes());
        out.extend_from_slice(&((ts % 1_000_000) as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(orig as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// Target devs of a running guest's interfaces.
fn taps(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let end = block.find("</interface>").unwrap_or(block.len());
        let (dev, _, _) = super::domains::interface_info(&block[..end]);
        if let Some(d) = dev {
            out.push(d);
        }
        rest = &block[end.max(1)..];
    }
    out
}

fn rand_id() -> u64 {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        return now_us();
    }
    u64::from_le_bytes(b)
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0x52, 0x54, 0, 0, 0, 1, 0x52, 0x54, 0, 0, 0, 2];
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    fn ip4(proto: u8, src: [u8; 4], dst: [u8; 4], t: &[u8]) -> Vec<u8> {
        let mut p = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, proto, 0, 0];
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        p.extend_from_slice(t);
        p
    }

    #[test]
    fn a_tcp_syn_is_summarised_with_ports_and_flags() {
        let mut tcp = vec![
            0x9c, 0x40, 0x01, 0xbb, 0, 0, 0, 1, 0, 0, 0, 0, 0x50, 0x02, 0xff, 0xff, 0, 0, 0, 0,
        ];
        tcp[13] = 0x02;
        let l = decode(
            &eth(0x0800, &ip4(6, [10, 0, 0, 5], [140, 82, 112, 3], &tcp)),
            "out",
            "vnet1",
            7,
        )
        .unwrap();
        assert_eq!(
            (l.proto.as_str(), l.src.as_str(), l.dst.as_str()),
            ("tcp", "10.0.0.5", "140.82.112.3")
        );
        assert_eq!(
            (l.sport, l.dport, l.info.as_str()),
            (Some(40000), Some(443), "SYN")
        );
        assert_eq!((l.dir, l.ts_us), ("out", 7));
    }

    #[test]
    fn dns_queries_and_answers_show_the_name_and_addresses() {
        // Built with the resolver's own encoder, so both ends agree.
        let q = crate::ipam::dns::tests::query("github.com", 1);
        let mut udp = vec![0xc3, 0x50, 0, 53, 0, 0, 0, 0];
        udp.extend_from_slice(&q);
        let l = decode(
            &eth(0x0800, &ip4(17, [10, 0, 0, 5], [10, 0, 0, 1], &udp)),
            "out",
            "vnet1",
            0,
        )
        .unwrap();
        assert_eq!(
            (l.proto.as_str(), l.info.as_str()),
            ("dns", "A? github.com")
        );
        let question = crate::ipam::dns::question(&q).unwrap();
        let ans = crate::ipam::dns::respond(
            &q,
            &question,
            &crate::ipam::dns::Local::A(vec!["140.82.112.3".parse().unwrap()]),
        );
        let mut udp = vec![0, 53, 0xc3, 0x50, 0, 0, 0, 0];
        udp.extend_from_slice(&ans);
        let l = decode(
            &eth(0x0800, &ip4(17, [10, 0, 0, 1], [10, 0, 0, 5], &udp)),
            "in",
            "vnet1",
            0,
        )
        .unwrap();
        assert_eq!(l.info, "A github.com → 140.82.112.3");
    }

    #[test]
    fn arp_icmp_dhcp_and_junk() {
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&[0x52, 0x54, 0, 0, 0, 2, 10, 0, 0, 5]);
        a.extend_from_slice(&[0, 0, 0, 0, 0, 0, 10, 0, 0, 1]);
        let l = decode(&eth(0x0806, &a), "out", "t", 0).unwrap();
        assert_eq!(
            (l.proto.as_str(), l.info.as_str()),
            ("arp", "who has 10.0.0.1? tell 10.0.0.5")
        );
        let l = decode(
            &eth(0x0800, &ip4(1, [10, 0, 0, 5], [1, 1, 1, 1], &[8, 0, 0, 0])),
            "out",
            "t",
            0,
        )
        .unwrap();
        assert_eq!(
            (l.proto.as_str(), l.info.as_str()),
            ("icmp", "echo request")
        );
        let l = decode(
            &eth(
                0x0800,
                &ip4(17, [0; 4], [255; 4], &[0, 68, 0, 67, 0, 0, 0, 0]),
            ),
            "out",
            "t",
            0,
        )
        .unwrap();
        assert_eq!(l.proto, "dhcp");
        assert!(decode(&[0; 10], "out", "t", 0).is_none());
        // Captured on a guest tap: the bridge's STP hello, an 802.3 frame.
        let mut stp = vec![
            0x01, 0x80, 0xc2, 0, 0, 0, 0xfe, 0x54, 0, 0xd4, 0xc0, 0x21, 0x00, 0x26, 0x42, 0x42,
            0x03,
        ];
        stp.extend_from_slice(&[0; 35]);
        assert_eq!(decode(&stp, "in", "t", 0).unwrap().proto, "stp");
        assert_eq!(
            decode(&eth(0x0800, &[0x45, 0]), "out", "t", 0)
                .unwrap()
                .proto,
            "ipv4?",
            "truncated IP does not panic"
        );
        let short_tcp = ip4(6, [1; 4], [2; 4], &[0, 80]);
        assert_eq!(
            decode(&eth(0x0800, &short_tcp), "out", "t", 0)
                .unwrap()
                .dport,
            None
        );
    }

    #[test]
    fn the_filter_keeps_only_what_was_asked_for() {
        let mut tcp = vec![
            0x9c, 0x40, 0x01, 0xbb, 0, 0, 0, 1, 0, 0, 0, 0, 0x50, 0x10, 0, 0, 0, 0, 0, 0,
        ];
        tcp[13] = 0x10;
        let l = decode(
            &eth(0x0800, &ip4(6, [10, 0, 0, 5], [140, 82, 112, 3], &tcp)),
            "out",
            "t",
            0,
        )
        .unwrap();
        assert!(Filter::default().keeps(&l));
        assert!(
            Filter {
                proto: Some("tcp".into()),
                port: Some(443),
                host: Some("140.82.112.3".parse().unwrap())
            }
            .keeps(&l)
        );
        assert!(
            !Filter {
                proto: Some("udp".into()),
                ..Default::default()
            }
            .keeps(&l)
        );
        assert!(
            !Filter {
                port: Some(80),
                ..Default::default()
            }
            .keeps(&l)
        );
        assert!(
            !Filter {
                host: Some("1.1.1.1".parse().unwrap()),
                ..Default::default()
            }
            .keeps(&l)
        );
    }

    #[test]
    fn the_pcap_file_is_what_wireshark_reads() {
        let f = eth(0x0806, &[0; 28]);
        let p = write_pcap([(1_700_000_000_123_456u64, 1500usize, f.as_slice())].into_iter());
        assert_eq!(&p[..4], &0xa1b2_c3d4u32.to_le_bytes(), "magic");
        assert_eq!(
            u32::from_le_bytes(p[20..24].try_into().unwrap()),
            1,
            "Ethernet"
        );
        assert_eq!(
            u32::from_le_bytes(p[24..28].try_into().unwrap()),
            1_700_000_000,
            "seconds"
        );
        assert_eq!(
            u32::from_le_bytes(p[28..32].try_into().unwrap()),
            123_456,
            "microseconds"
        );
        assert_eq!(
            u32::from_le_bytes(p[32..36].try_into().unwrap()) as usize,
            f.len(),
            "captured length"
        );
        assert_eq!(
            u32::from_le_bytes(p[36..40].try_into().unwrap()),
            1500,
            "original length"
        );
        assert_eq!(p.len(), 24 + 16 + f.len());
    }

    #[test]
    fn a_guests_taps_are_read_from_its_live_xml() {
        let x = "<devices><interface type='network'><target dev='vnet3'/></interface>
                 <interface type='network'><source network='x'/></interface>
                 <interface type='bridge'><target dev='vnet9'/></interface></devices>";
        assert_eq!(taps(x), vec!["vnet3", "vnet9"]);
    }
}
