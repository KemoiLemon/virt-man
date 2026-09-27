//! Moving IPAM out of libvirt's network XML and back again.
//!
//! libvirt runs dnsmasq for a network whose definition has an IPv4 `<ip>` and
//! does not disable DNS. We take such a network over by storing its `<dhcp>`,
//! `<dns>` forwarders and `<domain>` in the IPAM store and defining it with
//! `<dns enable='no'/>` and no `<dhcp>`: libvirt then starts no dnsmasq, and
//! our servers answer instead.
//!
//! The network editor still speaks libvirt XML, so the API hands it the XML
//! with the stored IPAM put back in (`inject`), and takes it out again on save
//! (`strip` + `merge`). Editing a range or a reservation that way changes only
//! the store; the libvirt definition, and so the running network, is untouched.

use super::store::{Net, Reservation, import};

/// Whether libvirt would run dnsmasq for this definition, so we should take it
/// over. A network without IPv4 addressing (bridge mode, an isolated network
/// with no `<ip>`) has nothing for us to serve and is left alone.
pub fn needs_takeover(xml: &str) -> bool {
    import(xml).is_some() && !dns_disabled(xml)
}

/// Ours already: DNS off, so libvirt runs no dnsmasq.
pub fn is_managed(xml: &str) -> bool {
    import(xml).is_some() && dns_disabled(xml)
}

fn dns_disabled(xml: &str) -> bool {
    tag_at(xml, "<dns").is_some_and(|t| crate::disks::attr(t, "enable").is_some_and(|v| v == "no"))
}

/// The definition libvirt should hold: no `<dhcp>`, no `<domain>`, DNS off.
pub fn strip(xml: &str) -> String {
    let mut x = remove_element(xml, "dhcp");
    x = remove_element(&x, "dns");
    x = remove_element(&x, "domain");
    // libvirt wants <dns> before <ip>; put it just before the first <ip.
    match x.find("<ip") {
        Some(i) => format!("{}<dns enable='no'/>\n  {}", &x[..i], &x[i..]),
        None => x,
    }
}

/// The definition as the editor should see it: libvirt's, with our range,
/// reservations, forwarders and domain in the places libvirt keeps them.
pub fn inject(xml: &str, net: &Net) -> String {
    let mut x = remove_element(xml, "dhcp");
    x = remove_element(&x, "dns");
    x = remove_element(&x, "domain");
    let mut before_ip = String::new();
    if let Some(d) = net.domain.as_deref().filter(|d| !d.is_empty()) {
        before_ip.push_str(&format!("<domain name='{}'/>\n  ", esc(d)));
    }
    if !net.dns_forwarders.is_empty() {
        before_ip.push_str("<dns>");
        for f in &net.dns_forwarders {
            before_ip.push_str(&format!("<forwarder addr='{f}'/>"));
        }
        before_ip.push_str("</dns>\n  ");
    }
    let mut dhcp = format!(
        "<dhcp><range start='{}' end='{}'/>",
        net.range_start, net.range_end
    );
    for r in &net.reservations {
        dhcp.push_str(&format!("<host mac='{}' ip='{}'", r.mac, r.ip));
        if let Some(n) = &r.name {
            dhcp.push_str(&format!(" name='{}'", esc(n)));
        }
        dhcp.push_str("/>");
    }
    dhcp.push_str("</dhcp>");

    let Some(i) = x.find("<ip") else { return x };
    let Some(rel) = x[i..].find('>') else {
        return x;
    };
    let open_end = i + rel;
    let (head, tail) = if x[..open_end].ends_with('/') {
        // <ip .../> becomes <ip ...>…</ip>
        (
            format!("{}>{dhcp}</ip>", &x[i..open_end - 1]),
            &x[open_end + 1..],
        )
    } else {
        (format!("{}{dhcp}", &x[i..=open_end]), &x[open_end + 1..])
    };
    format!("{}{before_ip}{head}{tail}", &x[..i])
}

/// Fold what an edited definition says into the stored state.
///
/// The edited XML is the user's intent for the range, forwarders, domain and
/// the set of reservations. A reservation that comes back unchanged (same MAC
/// and address) keeps what the editor cannot see — whether the DHCP server
/// made it, which guest owns it, when it was last used — so saving the network
/// page does not turn every automatic reservation into a manual one.
pub fn merge(existing: Option<&Net>, edited_xml: &str) -> Option<Net> {
    let mut next = import(edited_xml)?;
    if let Some(old) = existing {
        next.lease_secs = old.lease_secs;
        for r in &mut next.reservations {
            if let Some(o) = old
                .reservations
                .iter()
                .find(|o| o.mac == r.mac && o.ip == r.ip)
            {
                *r = Reservation {
                    name: r.name.clone().or_else(|| o.name.clone()),
                    ..o.clone()
                };
            }
        }
    }
    Some(next)
}

/// A definition minus what libvirt generates on its own (uuid, MAC, the
/// bridge's stp/delay defaults), for telling a real change from a no-op.
pub fn strip_generated(xml: &str) -> String {
    let mut x = remove_element(xml, "uuid");
    x = remove_element(&x, "mac");
    for a in [" stp='on'", " stp=\"on\"", " delay='0'", " delay=\"0\""] {
        x = x.replace(a, "");
    }
    collapse_empty(&x)
}

/// `<ip …></ip>` to `<ip …/>`: the editor writes an element with no children
/// one way and libvirt the other, and they mean the same.
fn collapse_empty(xml: &str) -> String {
    let mut out = xml.to_string();
    let mut from = 0;
    while let Some(rel) = out[from..].find("></") {
        let i = from + rel;
        let Some(end) = out[i + 3..].find('>') else {
            break;
        };
        let name = out[i + 3..i + 3 + end].to_string();
        let open = out[..i].rfind('<');
        let matches = open.is_some_and(|o| {
            out[o + 1..].starts_with(&name)
                && out[o + 1 + name.len()..].starts_with([' ', '\t', '\n', '>'])
        });
        if matches {
            out.replace_range(i..i + 3 + end + 1, "/>");
            from = i;
        } else {
            from = i + 3;
        }
    }
    out
}

fn tag_at<'a>(xml: &'a str, start: &str) -> Option<&'a str> {
    tag_pos(xml, start).map(|(i, len)| &xml[i..i + len])
}

/// Position and length of the first `start` tag that is a whole element name.
fn tag_pos(xml: &str, start: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = xml[from..].find(start) {
        let i = from + rel;
        // `<dns` must not match `<dnsmasq…` or similar.
        let next = xml[i + start.len()..].chars().next();
        if matches!(next, Some(' ' | '>' | '/' | '\n' | '\t')) {
            let e = xml[i..].find('>')?;
            return Some((i, e + 1));
        }
        from = i + start.len();
    }
    None
}

/// Remove every `<name …/>` or `<name …>…</name>` element.
fn remove_element(xml: &str, name: &str) -> String {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut out = xml.to_string();
    while let Some((i, len)) = tag_pos(&out, &open) {
        let end = if out[i..i + len].ends_with("/>") {
            i + len
        } else {
            match out[i..].find(&close) {
                Some(c) => i + c + close.len(),
                None => i + len,
            }
        };
        // Take the element's own line with it, so the XML stays tidy.
        let start = out[..i]
            .rfind('\n')
            .filter(|&n| out[n..i].trim().is_empty())
            .unwrap_or(i);
        out.replace_range(start..end, "");
    }
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('\'', "&apos;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBVIRT: &str = "<network>
  <name>internet</name>
  <forward mode='nat'/>
  <bridge name='virbr1' stp='on' delay='0'/>
  <domain name='lab'/>
  <dns>
    <forwarder addr='1.1.1.1'/>
  </dns>
  <ip address='192.168.167.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='192.168.167.100' end='192.168.167.199'/>
      <host mac='52:54:00:82:de:19' name='alpine' ip='192.168.167.2'/>
    </dhcp>
  </ip>
</network>";

    #[test]
    fn a_dnsmasq_network_is_taken_over_and_a_managed_one_is_not_again() {
        assert!(needs_takeover(LIBVIRT));
        let s = strip(LIBVIRT);
        assert!(
            !s.contains("<dhcp")
                && !s.contains("<host")
                && !s.contains("<domain")
                && !s.contains("forwarder"),
            "{s}"
        );
        assert!(s.contains("<dns enable='no'/>"));
        assert!(
            s.find("<dns").unwrap() < s.find("<ip").unwrap(),
            "libvirt wants <dns> before <ip>"
        );
        assert!(
            s.contains("<bridge name='virbr1'") && s.contains("<forward mode='nat'/>"),
            "the rest is untouched"
        );
        assert!(!needs_takeover(&s) && is_managed(&s));
        assert_eq!(
            strip(&s).matches("<dns").count(),
            1,
            "stripping twice adds nothing"
        );
    }

    #[test]
    fn networks_with_nothing_to_serve_are_left_to_libvirt() {
        assert!(!needs_takeover(
            "<network><name>b</name><forward mode='bridge'/><bridge name='br0'/></network>"
        ));
        assert!(!needs_takeover(
            "<network><name>internal</name><bridge name='virbr2'/></network>"
        ));
    }

    #[test]
    fn the_editor_sees_the_stored_ipam_in_libvirts_shape() {
        let net = import(LIBVIRT).unwrap();
        let x = inject(&strip(LIBVIRT), &net);
        assert!(!x.contains("enable='no'"));
        let back = import(&x).unwrap();
        assert_eq!(back, net, "inject then import round-trips:\n{x}");
        // A self-closing <ip/> gains a body.
        let bare = "<network><name>n</name><dns enable='no'/><ip address='10.0.0.1' prefix='24'/></network>";
        let y = inject(bare, &import(bare).unwrap());
        assert!(
            y.contains("<ip address='10.0.0.1' prefix='24'><dhcp>") && y.contains("</dhcp></ip>"),
            "{y}"
        );
    }

    #[test]
    fn saving_the_network_page_keeps_what_the_dhcp_server_knew() {
        let mut old = import(LIBVIRT).unwrap();
        old.reservations.push(Reservation {
            mac: "52:54:00:00:00:09".into(),
            ip: "192.168.167.100".parse().unwrap(),
            name: Some("web".into()),
            dynamic: true,
            vm: Some("uuid-9".into()),
            seen_ms: 42,
        });
        // The editor round-trip: it gets the injected XML and sends it back
        // with the range changed and alpine's reservation removed.
        let edited = inject(&strip(LIBVIRT), &old)
            .replace(
                "<host mac='52:54:00:82:de:19' ip='192.168.167.2' name='alpine'/>",
                "",
            )
            .replace("192.168.167.199", "192.168.167.150");
        let m = merge(Some(&old), &edited).unwrap();
        assert_eq!(
            m.range_end,
            "192.168.167.150".parse::<std::net::Ipv4Addr>().unwrap()
        );
        assert_eq!(m.reservations.len(), 1, "a removed reservation is gone");
        let r = &m.reservations[0];
        assert_eq!(
            (r.dynamic, r.vm.as_deref(), r.seen_ms),
            (true, Some("uuid-9"), 42),
            "kept its provenance"
        );
        // A reservation moved to another address is the user's now.
        let moved = inject(&strip(LIBVIRT), &old).replace("192.168.167.100'", "192.168.167.120'");
        let m = merge(Some(&old), &moved).unwrap();
        let r = m
            .reservations
            .iter()
            .find(|r| r.mac == "52:54:00:00:00:09")
            .unwrap();
        assert!(!r.dynamic && r.vm.is_none());
    }

    #[test]
    fn what_libvirt_fills_in_is_not_a_change() {
        let mine = "<network><name>n</name><forward mode='nat'/><bridge name='virbr9'/><dns enable='no'/><ip address='10.0.0.1' prefix='24'/></network>";
        let libvirt = "<network>\n  <name>n</name>\n  <uuid>5b92a1a5</uuid>\n  <forward mode='nat'/>\n  <bridge name='virbr9' stp='on' delay='0'/>\n  <mac address='52:54:00:7f:1d:d5'/>\n  <dns enable='no'/>\n  <ip address='10.0.0.1' prefix='24'/>\n</network>";
        let n = |x: &str| {
            strip_generated(x)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };
        assert_eq!(n(mine), n(libvirt));
        assert_ne!(n(mine), n(&libvirt.replace("10.0.0.1", "10.0.1.1")));
        // What the SPA's editor sends for the same network.
        let editor = "<network>\n  <name>n</name>\n  <forward mode=\"nat\"/>\n  <bridge name=\"virbr9\"/>\n  <dns enable='no'/>\n  <ip address=\"10.0.0.1\" prefix=\"24\"></ip>\n</network>";
        assert_eq!(n(mine).replace('"', "'"), n(editor).replace('"', "'"));
        assert_eq!(collapse_empty("<a><b x='1'></b></a>"), "<a><b x='1'/></a>");
        assert_eq!(
            collapse_empty("<a><b>t</b></a>"),
            "<a><b>t</b></a>",
            "text is kept"
        );
    }

    #[test]
    fn names_are_escaped_in_the_xml() {
        let mut n = import(LIBVIRT).unwrap();
        n.domain = Some("a'b".into());
        assert!(inject(&strip(LIBVIRT), &n).contains("<domain name='a&apos;b'/>"));
    }
}
