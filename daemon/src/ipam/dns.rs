//! DNS for guest networks: guest names answered here, the rest forwarded.
//!
//! What dnsmasq did for a libvirt network, and no more: an `A` record for each
//! reservation's hostname (bare, and under the network's domain), the matching
//! `PTR` for its address, and every other question relayed unchanged to the
//! network's forwarders or the host's own resolvers. A name under the network's
//! domain that is not a guest is answered NXDOMAIN here rather than leaked
//! upstream, which is what a local domain means.
//!
//! The decision is pure (`local_answer`); `server.rs` does the sockets and the
//! forwarding.

use std::net::Ipv4Addr;

use super::store::Net;

pub const PORT: u16 = 53;
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const RCODE_NXDOMAIN: u8 = 3;
/// Guest records change when a reservation does; keep caches short.
const TTL: u32 = 60;

/// The one question of a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub id: u16,
    /// Lowercase, no trailing dot.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Byte offset just past the question: where answers go.
    pub end: usize,
}

/// Read the header and first question of a standard query. None for
/// responses, other opcodes, or anything malformed, which is then forwarded
/// untouched and left for the upstream server to judge.
pub fn question(b: &[u8]) -> Option<Question> {
    if b.len() < 12 || b[2] & 0x80 != 0 || (b[2] >> 3) & 0x0f != 0 {
        return None;
    }
    if u16::from_be_bytes([b[4], b[5]]) != 1 {
        return None;
    }
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let len = *b.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        // No compression pointers in a question we answer ourselves.
        if len & 0xc0 != 0 || labels.len() > 127 {
            return None;
        }
        let l = b.get(i + 1..i + 1 + len)?;
        labels.push(String::from_utf8_lossy(l).to_ascii_lowercase());
        i += 1 + len;
    }
    let qtype = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
    let qclass = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]);
    Some(Question {
        id: u16::from_be_bytes([b[0], b[1]]),
        name: labels.join("."),
        qtype,
        qclass,
        end: i + 4,
    })
}

/// What to do with a query.
#[derive(Debug, Clone, PartialEq)]
pub enum Local {
    /// Ours: answer with these A records (may be empty: the name exists but
    /// has no record of this type, e.g. AAAA for a guest).
    A(Vec<Ipv4Addr>),
    Ptr(String),
    /// Under our domain, or a reverse name in our subnet, but nobody's.
    NxDomain,
    /// Not ours: send upstream.
    Forward,
}

pub fn local_answer(q: &Question, net: &Net) -> Local {
    if q.qclass != CLASS_IN {
        return Local::Forward;
    }
    let domain = net.domain.as_deref().map(str::to_ascii_lowercase);
    // A bare name, or name.<domain>.
    let host = match &domain {
        Some(d) if q.name.ends_with(&format!(".{d}")) => {
            Some(&q.name[..q.name.len() - d.len() - 1])
        }
        _ if !q.name.contains('.') => Some(q.name.as_str()),
        _ => None,
    };
    if let Some(h) = host {
        let hits: Vec<Ipv4Addr> = net
            .reservations
            .iter()
            .filter(|r| r.name.as_deref() == Some(h))
            .map(|r| r.ip)
            .collect();
        if !hits.is_empty() {
            return match q.qtype {
                TYPE_A | TYPE_ANY => Local::A(hits),
                // The name exists; it just has no such record.
                _ => Local::A(Vec::new()),
            };
        }
        let under_domain = domain
            .as_deref()
            .is_some_and(|d| q.name.ends_with(&format!(".{d}")) || q.name == d);
        if under_domain {
            return Local::NxDomain;
        }
        // A bare, unknown single label: upstream may know it (search domains).
        return Local::Forward;
    }
    if (q.qtype == TYPE_PTR || q.qtype == TYPE_ANY)
        && let Some(ip) = reverse_ip(&q.name)
        && (net.contains_host(ip) || ip == net.gateway)
    {
        return match net.reservation_for_ip(ip).and_then(|r| r.name.clone()) {
            Some(n) => Local::Ptr(match &domain {
                Some(d) => format!("{n}.{d}"),
                None => n,
            }),
            None => Local::NxDomain,
        };
    }
    Local::Forward
}

/// `4.3.2.1.in-addr.arpa` to 1.2.3.4.
fn reverse_ip(name: &str) -> Option<Ipv4Addr> {
    let rest = name.strip_suffix(".in-addr.arpa")?;
    let parts: Vec<u8> = rest
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [d, c, b, a] = parts[..] else { return None };
    Some(Ipv4Addr::new(a, b, c, d))
}

/// Build the response to `query` from a local answer.
pub fn respond(query: &[u8], q: &Question, ans: &Local) -> Vec<u8> {
    let mut b = query[..q.end].to_vec();
    // QR=1, keep opcode and RD, set AA (we are the authority for these names)
    // and RA (we recurse for the rest by forwarding).
    b[2] = 0x80 | (query[2] & 0x79) | 0x04;
    b[3] = 0x80;
    for x in &mut b[6..12] {
        *x = 0; // counts: rewritten below
    }
    let name_ptr = [0xc0, 0x0c]; // the question's name, at offset 12
    let mut count = 0u16;
    match ans {
        Local::A(ips) => {
            for ip in ips {
                b.extend_from_slice(&name_ptr);
                b.extend_from_slice(&TYPE_A.to_be_bytes());
                b.extend_from_slice(&CLASS_IN.to_be_bytes());
                b.extend_from_slice(&TTL.to_be_bytes());
                b.extend_from_slice(&4u16.to_be_bytes());
                b.extend_from_slice(&ip.octets());
                count += 1;
            }
        }
        Local::Ptr(target) => {
            let mut rdata = Vec::new();
            for l in target.split('.').filter(|l| !l.is_empty()) {
                rdata.push(l.len().min(63) as u8);
                rdata.extend_from_slice(&l.as_bytes()[..l.len().min(63)]);
            }
            rdata.push(0);
            b.extend_from_slice(&name_ptr);
            b.extend_from_slice(&TYPE_PTR.to_be_bytes());
            b.extend_from_slice(&CLASS_IN.to_be_bytes());
            b.extend_from_slice(&TTL.to_be_bytes());
            b.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            b.extend_from_slice(&rdata);
            count = 1;
        }
        Local::NxDomain => b[3] |= RCODE_NXDOMAIN,
        Local::Forward => {}
    }
    b[4..6].copy_from_slice(&1u16.to_be_bytes());
    b[6..8].copy_from_slice(&count.to_be_bytes());
    b
}

/// A SERVFAIL for a query no upstream answered, so the guest fails fast
/// instead of waiting out its own timeout.
pub fn servfail(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut b = query.to_vec();
    b[2] = 0x80 | (query[2] & 0x79);
    b[3] = 0x80 | 2;
    Some(b)
}

/// The IPv4 addresses a response gives for its question, following the
/// CNAME chain as a resolver does (every A in the answer section counts).
/// Empty for anything malformed.
pub fn a_records(b: &[u8]) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if b.len() < 12 {
        return out;
    }
    let qd = u16::from_be_bytes([b[4], b[5]]);
    let an = u16::from_be_bytes([b[6], b[7]]);
    let mut i = 12;
    for _ in 0..qd {
        let Some(n) = skip_name(b, i) else { return out };
        i = n + 4;
    }
    for _ in 0..an {
        let Some(n) = skip_name(b, i) else { return out };
        let Some(h) = b.get(n..n + 10) else {
            return out;
        };
        let ty = u16::from_be_bytes([h[0], h[1]]);
        let len = u16::from_be_bytes([h[8], h[9]]) as usize;
        let Some(rd) = b.get(n + 10..n + 10 + len) else {
            return out;
        };
        if ty == TYPE_A && len == 4 {
            out.push(Ipv4Addr::new(rd[0], rd[1], rd[2], rd[3]));
        }
        i = n + 10 + len;
    }
    out
}

/// Offset just past a (possibly compressed) name at `i`.
fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    for _ in 0..128 {
        let len = *b.get(i)?;
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 == 0xc0 {
            b.get(i + 1)?;
            return Some(i + 2);
        }
        i += 1 + len as usize;
    }
    None
}

/// Upstream resolvers for a network: its own forwarders, else the host's.
pub fn upstreams(net: &Net) -> Vec<std::net::SocketAddr> {
    let list: Vec<Ipv4Addr> = if net.dns_forwarders.is_empty() {
        host_resolvers()
    } else {
        net.dns_forwarders.clone()
    };
    list.into_iter()
        // Never forward to ourselves: that loops until the query times out.
        .filter(|ip| *ip != net.gateway && !ip.is_loopback())
        .map(|ip| std::net::SocketAddr::from((ip, PORT)))
        .collect()
}

fn host_resolvers() -> Vec<Ipv4Addr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|t| parse_resolv(&t))
        .unwrap_or_default()
}

fn parse_resolv(text: &str) -> Vec<Ipv4Addr> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|v| v.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipam::store::{Reservation, import};

    pub fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut b = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.') {
            b.push(l.len() as u8);
            b.extend_from_slice(l.as_bytes());
        }
        b.push(0);
        b.extend_from_slice(&qtype.to_be_bytes());
        b.extend_from_slice(&CLASS_IN.to_be_bytes());
        b
    }

    fn net() -> Net {
        let mut n =
            import("<network><domain name='lab'/><ip address='10.9.0.1' prefix='24'/></network>")
                .unwrap();
        n.reservations.push(Reservation {
            mac: "m1".into(),
            ip: "10.9.0.7".parse().unwrap(),
            name: Some("web".into()),
            dynamic: false,
            vm: None,
            seen_ms: 0,
        });
        n.reservations.push(Reservation {
            mac: "m2".into(),
            ip: "10.9.0.8".parse().unwrap(),
            name: None,
            dynamic: true,
            vm: None,
            seen_ms: 0,
        });
        n
    }

    fn ask(name: &str, t: u16) -> Local {
        local_answer(&question(&query(name, t)).unwrap(), &net())
    }

    #[test]
    fn guest_names_resolve_bare_and_under_the_domain_in_any_case() {
        let web: Ipv4Addr = "10.9.0.7".parse().unwrap();
        assert_eq!(ask("web", TYPE_A), Local::A(vec![web]));
        assert_eq!(ask("WEB.lab", TYPE_A), Local::A(vec![web]));
        assert_eq!(
            ask("web.lab", 28),
            Local::A(vec![]),
            "AAAA: exists, but no IPv6 record"
        );
    }

    #[test]
    fn unknown_names_under_our_domain_stop_here_the_rest_go_upstream() {
        assert_eq!(ask("nope.lab", TYPE_A), Local::NxDomain);
        assert_eq!(ask("example.org", TYPE_A), Local::Forward);
        assert_eq!(
            ask("printer", TYPE_A),
            Local::Forward,
            "a bare unknown name may be a search-domain name"
        );
    }

    #[test]
    fn reverse_lookups_in_our_subnet_are_answered_here() {
        assert_eq!(
            ask("7.0.9.10.in-addr.arpa", TYPE_PTR),
            Local::Ptr("web.lab".into())
        );
        assert_eq!(
            ask("8.0.9.10.in-addr.arpa", TYPE_PTR),
            Local::NxDomain,
            "reserved but unnamed"
        );
        assert_eq!(ask("8.8.8.8.in-addr.arpa", TYPE_PTR), Local::Forward);
    }

    #[test]
    fn a_response_is_a_well_formed_answer_to_the_same_query() {
        let qb = query("web.lab", TYPE_A);
        let q = question(&qb).unwrap();
        let r = respond(&qb, &q, &Local::A(vec!["10.9.0.7".parse().unwrap()]));
        assert_eq!(&r[0..2], &[0x12, 0x34], "same id");
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[2] & 0x01, 0x01, "RD echoed");
        assert_eq!(r[3] & 0x0f, 0, "NOERROR");
        assert_eq!(&r[6..8], &[0, 1], "one answer");
        assert_eq!(&r[r.len() - 4..], &[10, 9, 0, 7]);
        let nx = respond(&qb, &q, &Local::NxDomain);
        assert_eq!(nx[3] & 0x0f, RCODE_NXDOMAIN);
        let ptr_q = query("7.0.9.10.in-addr.arpa", TYPE_PTR);
        let p = respond(
            &ptr_q,
            &question(&ptr_q).unwrap(),
            &Local::Ptr("web.lab".into()),
        );
        assert!(p.ends_with(&[3, b'w', b'e', b'b', 3, b'l', b'a', b'b', 0]));
    }

    #[test]
    fn answers_are_read_back_out_of_a_response() {
        let qb = query("web.lab", TYPE_A);
        let q = question(&qb).unwrap();
        let ips = vec!["10.9.0.7".parse().unwrap(), "10.9.0.8".parse().unwrap()];
        assert_eq!(a_records(&respond(&qb, &q, &Local::A(ips.clone()))), ips);
        assert!(a_records(&respond(&qb, &q, &Local::NxDomain)).is_empty());
        assert!(a_records(&[0; 11]).is_empty());
        let mut cut = respond(&qb, &q, &Local::A(ips));
        cut.truncate(cut.len() - 3);
        assert_eq!(
            a_records(&cut).len(),
            1,
            "a truncated record is dropped, the rest kept"
        );
    }

    #[test]
    fn malformed_and_non_query_packets_are_not_answered_locally() {
        assert!(question(&[0; 5]).is_none());
        let mut resp = query("web", TYPE_A);
        resp[2] |= 0x80;
        assert!(question(&resp).is_none(), "a response");
        let mut ptr = query("web", TYPE_A);
        ptr[12] = 0xc0;
        assert!(
            question(&ptr).is_none(),
            "a compression pointer in the question"
        );
        let mut short = query("web", TYPE_A);
        short.truncate(short.len() - 2);
        assert!(question(&short).is_none());
    }

    #[test]
    fn upstreams_come_from_the_network_else_the_host_and_never_loop() {
        assert_eq!(
            parse_resolv(
                "# x\nnameserver 192.168.0.11\nnameserver ::1\nsearch a\nnameserver 1.1.1.1"
            ),
            vec![
                "192.168.0.11".parse::<Ipv4Addr>().unwrap(),
                "1.1.1.1".parse().unwrap()
            ]
        );
        let mut n = net();
        n.dns_forwarders = vec![
            "10.9.0.1".parse().unwrap(),
            "9.9.9.9".parse().unwrap(),
            "127.0.0.53".parse().unwrap(),
        ];
        assert_eq!(upstreams(&n), vec!["9.9.9.9:53".parse().unwrap()]);
    }
}
