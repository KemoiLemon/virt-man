//! DHCPv4 (RFC 2131/2132) for guest networks: just what a VM needs.
//!
//! One address per MAC, and it is permanent: a guest with a reservation gets
//! that address; a guest without one gets the lowest free address in the
//! network's range, and that becomes its reservation until the guest is
//! deleted. So the "lease" handed out is only how often the guest checks in,
//! never a reason for its address to change.
//!
//! The codec and the decision (`answer`) are pure, so they are tested with
//! real packet bytes; `server.rs` owns the sockets.

use std::net::Ipv4Addr;

use super::store::{Net, Reservation, clean_hostname};

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

// Message types (option 53).
pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const DECLINE: u8 = 4;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;
pub const RELEASE: u8 = 7;
pub const INFORM: u8 = 8;

// Options.
const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_HOSTNAME: u8 = 12;
const OPT_DOMAIN: u8 = 15;
const OPT_BROADCAST: u8 = 28;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_MESSAGE: u8 = 56;
const OPT_RENEWAL: u8 = 58;
const OPT_REBINDING: u8 = 59;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;
const OPT_PAD: u8 = 0;

/// The parts of a client message this server acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub op: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub msg_type: u8,
    pub requested_ip: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
    pub hostname: Option<String>,
    pub client_id: Option<Vec<u8>>,
}

impl Packet {
    pub fn mac(&self) -> String {
        self.chaddr[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Decode a BOOTREQUEST. None for anything that is not a well-formed
/// Ethernet DHCP request (replies from another server, BOOTP without the
/// magic cookie, truncated packets).
pub fn parse(b: &[u8]) -> Option<Packet> {
    if b.len() < 240 || b[0] != 1 || b[1] != 1 || b[2] != 6 || b[236..240] != MAGIC {
        return None;
    }
    let ip = |o: usize| Ipv4Addr::new(b[o], b[o + 1], b[o + 2], b[o + 3]);
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&b[28..44]);
    let mut p = Packet {
        op: b[0],
        xid: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        secs: u16::from_be_bytes([b[8], b[9]]),
        flags: u16::from_be_bytes([b[10], b[11]]),
        ciaddr: ip(12),
        yiaddr: ip(16),
        siaddr: ip(20),
        giaddr: ip(24),
        chaddr,
        msg_type: 0,
        requested_ip: None,
        server_id: None,
        hostname: None,
        client_id: None,
    };
    let mut i = 240;
    while i < b.len() {
        let code = b[i];
        if code == OPT_END {
            break;
        }
        if code == OPT_PAD {
            i += 1;
            continue;
        }
        let len = *b.get(i + 1)? as usize;
        let v = b.get(i + 2..i + 2 + len)?;
        match code {
            OPT_MSG_TYPE if len == 1 => p.msg_type = v[0],
            OPT_REQUESTED_IP if len == 4 => {
                p.requested_ip = Some(Ipv4Addr::new(v[0], v[1], v[2], v[3]))
            }
            OPT_SERVER_ID if len == 4 => p.server_id = Some(Ipv4Addr::new(v[0], v[1], v[2], v[3])),
            OPT_HOSTNAME => p.hostname = Some(String::from_utf8_lossy(v).into_owned()),
            OPT_CLIENT_ID => p.client_id = Some(v.to_vec()),
            _ => {}
        }
        i += 2 + len;
    }
    (p.msg_type != 0).then_some(p)
}

/// What to send back, before encoding.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub msg_type: u8,
    pub yiaddr: Ipv4Addr,
    pub message: Option<String>,
}

/// What `answer` wants done to the store. The caller persists it.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    None,
    /// A new permanent reservation, made from the range.
    Reserve(Reservation),
    /// The guest was given its address: record when, and a hostname it told us
    /// if the reservation had none.
    Seen {
        mac: String,
        name: Option<String>,
    },
}

/// Decide the reply to one client message.
///
/// `vm_of` names the guest (libvirt UUID) owning a MAC, if it is one of ours;
/// `in_use` says whether an address answers on the wire already. Only guests
/// of this host are served: a MAC no guest owns (another DHCP client bridged
/// in) gets no answer, rather than an address out of our range.
pub fn answer(
    p: &Packet,
    net: &Net,
    vm_of: &dyn Fn(&str) -> Option<(String, String)>,
    in_use: &dyn Fn(Ipv4Addr) -> bool,
    _now_ms: u64,
) -> (Option<Reply>, Change) {
    let mac = p.mac();
    let reserved = net.reservation_for_mac(&mac);
    let ours = |r: &Reservation| Reply {
        msg_type: 0,
        yiaddr: r.ip,
        message: None,
    };
    let seen = || Change::Seen {
        mac: mac.clone(),
        name: p.hostname.as_deref().and_then(clean_hostname),
    };
    let nak = |why: &str| {
        (
            Some(Reply {
                msg_type: NAK,
                yiaddr: Ipv4Addr::UNSPECIFIED,
                message: Some(why.into()),
            }),
            Change::None,
        )
    };

    match p.msg_type {
        DISCOVER => {
            if let Some(r) = reserved {
                return (
                    Some(Reply {
                        msg_type: OFFER,
                        ..ours(r)
                    }),
                    Change::None,
                );
            }
            let Some((uuid, vm_name)) = vm_of(&mac) else {
                return (None, Change::None);
            };
            let Some(ip) = net.next_free(in_use) else {
                // Out of addresses: say nothing, as dnsmasq does. The guest
                // retries, and the user sees the range is full in the UI.
                return (None, Change::None);
            };
            let r = Reservation {
                mac: mac.clone(),
                ip,
                name: p
                    .hostname
                    .as_deref()
                    .and_then(clean_hostname)
                    .or_else(|| clean_hostname(&vm_name)),
                dynamic: true,
                vm: Some(uuid),
                seen_ms: 0,
            };
            // Reserved at OFFER, not at ACK: a second DISCOVER from another
            // guest a moment later must not be offered the same address.
            (
                Some(Reply {
                    msg_type: OFFER,
                    yiaddr: ip,
                    message: None,
                }),
                Change::Reserve(r),
            )
        }
        REQUEST => {
            // SELECTING: the client names the server it chose. Not us: stay
            // quiet (another server on the segment is answering).
            if p.server_id.is_some_and(|s| s != net.gateway) {
                return (None, Change::None);
            }
            let wanted = p.requested_ip.unwrap_or(p.ciaddr);
            match reserved {
                Some(r) if r.ip == wanted || wanted.is_unspecified() => (
                    Some(Reply {
                        msg_type: ACK,
                        ..ours(r)
                    }),
                    seen(),
                ),
                // It asks for an address it does not own (moved networks, an
                // old lease): NAK so it starts over and DISCOVERs its own.
                Some(_) => nak("that address is not yours"),
                None if vm_of(&mac).is_some() => nak("no address reserved; start over"),
                None => (None, Change::None),
            }
        }
        INFORM => {
            // Has an address already, wants only the options.
            if reserved.is_some() || vm_of(&mac).is_some() {
                (
                    Some(Reply {
                        msg_type: ACK,
                        yiaddr: Ipv4Addr::UNSPECIFIED,
                        message: None,
                    }),
                    Change::None,
                )
            } else {
                (None, Change::None)
            }
        }
        // A release or a decline changes nothing: the address stays reserved to
        // this guest, which is the whole point. (A decline means something else
        // answered on that address; the user has to sort that out, and handing
        // this guest a different address would only hide it.)
        RELEASE | DECLINE => (None, Change::None),
        _ => (None, Change::None),
    }
}

/// Encode a reply to `req`. `net` supplies the options every guest needs.
pub fn encode(req: &Packet, reply: &Reply, net: &Net, hostname: Option<&str>) -> Vec<u8> {
    let mut b = vec![0u8; 240];
    b[0] = 2; // BOOTREPLY
    b[1] = 1;
    b[2] = 6;
    b[4..8].copy_from_slice(&req.xid.to_be_bytes());
    b[10..12].copy_from_slice(&req.flags.to_be_bytes());
    if reply.msg_type == ACK && reply.yiaddr.is_unspecified() {
        // INFORM: echo the address the client already has.
        b[12..16].copy_from_slice(&req.ciaddr.octets());
    }
    b[16..20].copy_from_slice(&reply.yiaddr.octets());
    if reply.msg_type != NAK {
        b[20..24].copy_from_slice(&net.gateway.octets());
    }
    b[24..28].copy_from_slice(&req.giaddr.octets());
    b[28..44].copy_from_slice(&req.chaddr);
    b[236..240].copy_from_slice(&MAGIC);

    let mut opt = |code: u8, v: &[u8]| {
        b.push(code);
        b.push(v.len() as u8);
        b.extend_from_slice(v);
    };
    opt(OPT_MSG_TYPE, &[reply.msg_type]);
    opt(OPT_SERVER_ID, &net.gateway.octets());
    if reply.msg_type == NAK {
        if let Some(m) = &reply.message {
            opt(OPT_MESSAGE, &m.as_bytes()[..m.len().min(255)]);
        }
    } else {
        if reply.msg_type != ACK || !reply.yiaddr.is_unspecified() {
            // No lease on an INFORM answer (RFC 2131 4.3.5).
            let lease = net.lease_secs.max(60);
            opt(OPT_LEASE, &lease.to_be_bytes());
            opt(OPT_RENEWAL, &(lease / 2).to_be_bytes());
            opt(OPT_REBINDING, &(lease / 8 * 7).to_be_bytes());
        }
        opt(OPT_SUBNET, &net.netmask().octets());
        opt(OPT_BROADCAST, &net.broadcast().octets());
        opt(OPT_ROUTER, &net.gateway.octets());
        // Guests ask us; we answer their names and forward the rest.
        opt(OPT_DNS, &net.gateway.octets());
        if let Some(d) = net.domain.as_deref().filter(|d| !d.is_empty()) {
            opt(OPT_DOMAIN, &d.as_bytes()[..d.len().min(255)]);
        }
        if let Some(h) = hostname {
            opt(OPT_HOSTNAME, &h.as_bytes()[..h.len().min(255)]);
        }
    }
    b.push(OPT_END);
    // Some old clients drop replies shorter than a BOOTP packet (300 bytes).
    if b.len() < 300 {
        b.resize(300, 0);
    }
    b
}

/// Where the reply goes (RFC 2131 4.1): to a relay if there is one; to the
/// client's own address when it has one and is renewing; otherwise broadcast,
/// since a client with no address cannot receive unicast.
pub fn destination(req: &Packet, reply: &Reply) -> (Ipv4Addr, u16) {
    if !req.giaddr.is_unspecified() {
        return (req.giaddr, SERVER_PORT);
    }
    if reply.msg_type != NAK && !req.ciaddr.is_unspecified() {
        return (req.ciaddr, CLIENT_PORT);
    }
    (Ipv4Addr::BROADCAST, CLIENT_PORT)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::ipam::store::import;

    pub const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    /// A client message exactly as a guest puts it on the wire.
    pub fn client(
        msg: u8,
        requested: Option<[u8; 4]>,
        server: Option<[u8; 4]>,
        ciaddr: [u8; 4],
    ) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        b[0] = 1;
        b[1] = 1;
        b[2] = 6;
        b[4..8].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        b[10] = 0x80; // broadcast flag
        b[12..16].copy_from_slice(&ciaddr);
        b[28..34].copy_from_slice(&MAC);
        b[236..240].copy_from_slice(&MAGIC);
        b.extend_from_slice(&[OPT_MSG_TYPE, 1, msg]);
        if let Some(r) = requested {
            b.extend_from_slice(&[OPT_REQUESTED_IP, 4]);
            b.extend_from_slice(&r);
        }
        if let Some(s) = server {
            b.extend_from_slice(&[OPT_SERVER_ID, 4]);
            b.extend_from_slice(&s);
        }
        b.extend_from_slice(&[OPT_HOSTNAME, 6]);
        b.extend_from_slice(b"guest1");
        b.extend_from_slice(&[OPT_PAD, OPT_PAD, OPT_END]);
        b
    }

    fn net() -> Net {
        import("<network><ip address='10.9.0.1' prefix='24'><dhcp><range start='10.9.0.50' end='10.9.0.52'/></dhcp></ip></network>").unwrap()
    }
    fn guest(_: &str) -> Option<(String, String)> {
        Some(("uuid-1".into(), "web01".into()))
    }
    fn stranger(_: &str) -> Option<(String, String)> {
        None
    }
    fn free(_: Ipv4Addr) -> bool {
        false
    }

    #[test]
    fn a_real_discover_is_parsed() {
        let p = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        assert_eq!(p.msg_type, DISCOVER);
        assert_eq!(p.mac(), "52:54:00:12:34:56");
        assert_eq!(p.xid, 0xdead_beef);
        assert_eq!(p.hostname.as_deref(), Some("guest1"));
        assert_eq!(p.flags, 0x8000);
    }

    #[test]
    fn junk_is_ignored_not_panicked_on() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[1; 239]).is_none());
        let mut reply = client(DISCOVER, None, None, [0; 4]);
        reply[0] = 2;
        assert!(parse(&reply).is_none(), "a BOOTREPLY from another server");
        // An option claiming four bytes when one is left: rejected, not read
        // past the end of the buffer.
        let mut trunc = client(DISCOVER, None, None, [0; 4]);
        trunc.truncate(240);
        trunc.extend_from_slice(&[OPT_MSG_TYPE, 1, DISCOVER, OPT_REQUESTED_IP, 4, 10]);
        assert!(parse(&trunc).is_none());
    }

    #[test]
    fn a_new_guest_is_offered_the_first_free_address_and_it_becomes_permanent() {
        let n = net();
        let p = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        let (reply, change) = answer(&p, &n, &guest, &free, 1);
        assert_eq!(
            reply.unwrap(),
            Reply {
                msg_type: OFFER,
                yiaddr: "10.9.0.50".parse().unwrap(),
                message: None
            }
        );
        let Change::Reserve(r) = change else {
            panic!("{change:?}")
        };
        assert_eq!(
            (r.dynamic, r.vm.as_deref(), r.name.as_deref()),
            (true, Some("uuid-1"), Some("guest1"))
        );
    }

    #[test]
    fn a_reserved_guest_always_gets_its_address() {
        let mut n = net();
        n.reservations.push(Reservation {
            mac: "52:54:00:12:34:56".into(),
            ip: "10.9.0.7".parse().unwrap(),
            name: None,
            dynamic: false,
            vm: None,
            seen_ms: 0,
        });
        let d = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        let (r, c) = answer(&d, &n, &guest, &free, 1);
        assert_eq!(
            r.unwrap().yiaddr,
            "10.9.0.7".parse::<Ipv4Addr>().unwrap(),
            "outside the range is fine for a reservation"
        );
        assert_eq!(c, Change::None);
        let q = parse(&client(
            REQUEST,
            Some([10, 9, 0, 7]),
            Some([10, 9, 0, 1]),
            [0; 4],
        ))
        .unwrap();
        let (r, c) = answer(&q, &n, &guest, &free, 1);
        assert_eq!(r.unwrap().msg_type, ACK);
        assert!(matches!(c, Change::Seen { .. }));
        // Renewing: no requested-ip option, its address in ciaddr.
        let renew = parse(&client(REQUEST, None, None, [10, 9, 0, 7])).unwrap();
        assert_eq!(
            answer(&renew, &n, &guest, &free, 1).0.unwrap().msg_type,
            ACK
        );
    }

    #[test]
    fn asking_for_someone_elses_address_is_refused() {
        let mut n = net();
        n.reservations.push(Reservation {
            mac: "52:54:00:12:34:56".into(),
            ip: "10.9.0.7".parse().unwrap(),
            name: None,
            dynamic: false,
            vm: None,
            seen_ms: 0,
        });
        let q = parse(&client(REQUEST, Some([10, 9, 0, 8]), None, [0; 4])).unwrap();
        assert_eq!(answer(&q, &n, &guest, &free, 1).0.unwrap().msg_type, NAK);
        // A guest with no reservation that REQUESTs straight away (INIT-REBOOT
        // from a previous life) is told to start over, so it DISCOVERs.
        let q = parse(&client(REQUEST, Some([10, 9, 0, 99]), None, [0; 4])).unwrap();
        assert_eq!(
            answer(&q, &net(), &guest, &free, 1).0.unwrap().msg_type,
            NAK
        );
    }

    #[test]
    fn a_request_to_another_server_and_unknown_macs_get_silence() {
        let q = parse(&client(
            REQUEST,
            Some([10, 9, 0, 50]),
            Some([10, 9, 0, 254]),
            [0; 4],
        ))
        .unwrap();
        assert_eq!(answer(&q, &net(), &guest, &free, 1).0, None);
        let d = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        assert_eq!(
            answer(&d, &net(), &stranger, &free, 1),
            (None, Change::None)
        );
    }

    #[test]
    fn release_keeps_the_reservation() {
        let r = parse(&client(RELEASE, None, None, [10, 9, 0, 50])).unwrap();
        assert_eq!(answer(&r, &net(), &guest, &free, 1), (None, Change::None));
    }

    #[test]
    fn a_full_range_offers_nothing_rather_than_a_taken_address() {
        let d = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        let all = |_: Ipv4Addr| true;
        assert_eq!(answer(&d, &net(), &guest, &all, 1), (None, Change::None));
    }

    #[test]
    fn the_offer_carries_what_a_guest_needs_to_get_online() {
        let n = net();
        let d = parse(&client(DISCOVER, None, None, [0; 4])).unwrap();
        let reply = Reply {
            msg_type: OFFER,
            yiaddr: "10.9.0.50".parse().unwrap(),
            message: None,
        };
        let b = encode(&d, &reply, &n, Some("guest1"));
        assert!(b.len() >= 300);
        assert_eq!(b[0], 2, "BOOTREPLY");
        assert_eq!(&b[4..8], &0xdead_beefu32.to_be_bytes(), "same transaction");
        assert_eq!(&b[16..20], &[10, 9, 0, 50], "yiaddr");
        assert_eq!(&b[28..34], &MAC);
        let opts = options(&b);
        assert_eq!(opts.get(&OPT_MSG_TYPE), Some(&vec![OFFER]));
        assert_eq!(opts.get(&OPT_SERVER_ID), Some(&vec![10, 9, 0, 1]));
        assert_eq!(opts.get(&OPT_SUBNET), Some(&vec![255, 255, 255, 0]));
        assert_eq!(opts.get(&OPT_ROUTER), Some(&vec![10, 9, 0, 1]));
        assert_eq!(opts.get(&OPT_DNS), Some(&vec![10, 9, 0, 1]));
        assert_eq!(opts.get(&OPT_LEASE), Some(&3600u32.to_be_bytes().to_vec()));
        assert_eq!(opts.get(&OPT_HOSTNAME), Some(&b"guest1".to_vec()));
        assert_eq!(destination(&d, &reply), (Ipv4Addr::BROADCAST, CLIENT_PORT));
    }

    #[test]
    fn replies_go_where_the_client_can_hear_them() {
        let ack = Reply {
            msg_type: ACK,
            yiaddr: "10.9.0.50".parse().unwrap(),
            message: None,
        };
        let renew = parse(&client(REQUEST, None, None, [10, 9, 0, 50])).unwrap();
        assert_eq!(
            destination(&renew, &ack),
            ("10.9.0.50".parse().unwrap(), CLIENT_PORT)
        );
        let nak = Reply {
            msg_type: NAK,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            message: None,
        };
        assert_eq!(
            destination(&renew, &nak),
            (Ipv4Addr::BROADCAST, CLIENT_PORT),
            "a NAK is always broadcast"
        );
    }

    fn options(b: &[u8]) -> std::collections::HashMap<u8, Vec<u8>> {
        let mut m = std::collections::HashMap::new();
        let mut i = 240;
        while i < b.len() && b[i] != OPT_END {
            if b[i] == OPT_PAD {
                i += 1;
                continue;
            }
            let len = b[i + 1] as usize;
            m.insert(b[i], b[i + 2..i + 2 + len].to_vec());
            i += 2 + len;
        }
        m
    }
}
