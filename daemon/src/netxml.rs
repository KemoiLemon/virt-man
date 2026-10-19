//! The network editor's XML, rendered from the store and read back into it.
//!
//! The daemon owns these networks; the SPA still edits them as libvirt-shaped
//! XML (`networkXml`/`networkFormFromXml` in `web/src/xml.ts`). `render`
//! writes the stored state in that shape, `parse` reads it back through
//! `store::import` and rejects what a bridge or dnsmasq could not run, and
//! `merge` folds an edit into the stored state without losing what the editor
//! cannot show.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use crate::hostnet;
use crate::ipam::store::{self, Mode, Net};

/// The definition as the editor should see it: name and uuid (both the store
/// key), title, forward mode, bridge, DNS and the IPAM range and hosts.
pub fn render(id: &str, net: &Net) -> String {
    let forward = match net.mode {
        Mode::Nat => "nat",
        Mode::Bridge => "bridge",
        Mode::Isolated => "none",
    };
    let mut s = format!(
        "<network>\n  <name>{}</name>\n  <uuid>{}</uuid>\n",
        esc(id),
        esc(id)
    );
    if !net.title.is_empty() {
        s.push_str(&format!("  <title>{}</title>\n", esc(&net.title)));
    }
    s.push_str(&format!("  <forward mode=\"{forward}\"/>\n"));
    // The bridge is the derived one for a managed network, so the network
    // page's BRIDGE card and the editor show what actually runs, and the
    // operator's for a bridge network.
    let bridge = hostnet::bridge_for(net, id);
    if !bridge.is_empty() {
        s.push_str(&format!("  <bridge name=\"{}\"/>\n", esc(&bridge)));
    }
    if net.mode != Mode::Bridge
        && let Some(d) = net.domain.as_deref().filter(|d| !d.is_empty())
    {
        s.push_str(&format!("  <domain name=\"{}\"/>\n", esc(d)));
    }
    // Forwarders are a NAT setting: an isolated network has nowhere to send a
    // query, and a bridge network's DNS belongs to the operator's bridge.
    if net.mode == Mode::Nat && !net.dns_forwarders.is_empty() {
        s.push_str("  <dns>\n");
        for f in &net.dns_forwarders {
            s.push_str(&format!("    <forwarder addr=\"{f}\"/>\n"));
        }
        s.push_str("  </dns>\n");
    }
    if net.mode != Mode::Bridge && !net.gateway.is_unspecified() {
        let open = format!(
            "  <ip address=\"{}\" netmask=\"{}\"",
            net.gateway,
            net.netmask()
        );
        let mut dhcp = String::new();
        if !net.range_start.is_unspecified() && !net.range_end.is_unspecified() {
            dhcp.push_str(&format!(
                "<range start=\"{}\" end=\"{}\"/>",
                net.range_start, net.range_end
            ));
        }
        for r in &net.reservations {
            dhcp.push_str(&format!("<host mac=\"{}\" ip=\"{}\"", esc(&r.mac), r.ip));
            if let Some(name) = r.name.as_deref().filter(|n| !n.is_empty()) {
                dhcp.push_str(&format!(" name=\"{}\"", esc(name)));
            }
            dhcp.push_str("/>");
        }
        if dhcp.is_empty() {
            s.push_str(&format!("{open}/>\n"));
        } else {
            s.push_str(&format!("{open}>\n    <dhcp>{dhcp}</dhcp>\n  </ip>\n"));
        }
    }
    s.push_str("</network>\n");
    s
}

/// Read a definition and check it describes something this daemon can run.
pub fn parse(xml: &str) -> Result<Net, String> {
    let net = store::import(xml)?;
    validate(&net)?;
    Ok(net)
}

/// Check a network for what a bridge and dnsmasq need: a usable gateway, a
/// range that lies inside the subnet and never includes the gateway, and
/// reservations that are real hosts of it. A managed network may deliberately
/// have no subnet (an isolated segment, or a NAT one the API is about to give
/// an address); then there is nothing to validate and it must hold no
/// reservations.
fn validate(net: &Net) -> Result<(), String> {
    if net.mode == Mode::Bridge {
        if !hostnet::valid_name(&net.bridge) {
            return Err(format!("'{}' is not a usable bridge name", net.bridge));
        }
        return Ok(());
    }
    if net.gateway.is_unspecified() {
        if !net.reservations.is_empty() {
            return Err("a reservation needs a subnet to live in".into());
        }
        return Ok(());
    }
    if !(1..=30).contains(&net.prefix) {
        return Err(format!(
            "the prefix must be between 1 and 30, not {}",
            net.prefix
        ));
    }
    let subnet = format!(
        "{}/{}",
        Ipv4Addr::from(u32::from(net.gateway) & store::mask(net.prefix)),
        net.prefix
    );
    if !net.contains_host(net.gateway) {
        return Err(format!(
            "the gateway {} is not a host address in {subnet}",
            net.gateway
        ));
    }
    net.validate_range(net.range_start, net.range_end)?;
    let (a, b) = (u32::from(net.range_start), u32::from(net.range_end));
    if (a..=b).contains(&u32::from(net.gateway)) {
        return Err("the range must not include the gateway".into());
    }
    let mut macs = HashSet::new();
    let mut ips = HashSet::new();
    for r in &net.reservations {
        if store::normalize_mac(&r.mac).as_deref() != Some(r.mac.as_str()) {
            return Err(format!("'{}' is not a MAC address", r.mac));
        }
        if !macs.insert(r.mac.clone()) {
            return Err(format!("two reservations share the MAC {}", r.mac));
        }
        if !ips.insert(r.ip) {
            return Err(format!("two reservations share the address {}", r.ip));
        }
        if !net.contains_host(r.ip) {
            return Err(format!(
                "reservation {} is not a host address in {subnet}",
                r.ip
            ));
        }
        if r.ip == net.gateway {
            return Err(format!("reservation {} is the network's own gateway", r.ip));
        }
        if let Some(name) = &r.name
            && store::clean_hostname(name).as_deref() != Some(name.as_str())
        {
            return Err(format!("'{name}' is not a valid hostname"));
        }
    }
    Ok(())
}

/// Fold an edit into the stored state.
///
/// The editor cannot show autostart or the lease time, and it may not have
/// seen a reservation allocated since it was opened (a guest appearing on the
/// network gets one without anyone saving a form). Those survive the edit;
/// a reservation that comes back unchanged keeps whether the daemon made it,
/// which guest owns it, and when it was last used. A network that no longer has
/// a subnet keeps no reservations: an address outside a subnet cannot be
/// served, and dnsmasq refuses a dhcp-host for one.
pub fn merge(old: &Net, mut new: Net) -> Result<Net, String> {
    new.autostart = old.autostart;
    new.lease_secs = old.lease_secs;
    if new.subnet().is_none() {
        new.reservations.clear();
    } else {
        for r in &mut new.reservations {
            if let Some(o) = old.reservations.iter().find(|o| o.mac == r.mac) {
                r.dynamic = o.dynamic;
                r.vm = o.vm.clone();
                r.seen_ms = o.seen_ms;
                if r.name.is_none() {
                    r.name = o.name.clone();
                }
            }
        }
        for o in &old.reservations {
            if new.reservations.iter().any(|r| r.mac == o.mac) {
                continue;
            }
            // An address the new subnet no longer holds cannot be served.
            if !new.contains_host(o.ip) || o.ip == new.gateway {
                continue;
            }
            new.reservations.push(o.clone());
        }
    }
    validate(&new)?;
    Ok(new)
}

/// XML text and attribute escaping. One function for both: a value that is
/// safe in an attribute is safe in text, and dnsmasq and libvirt never see it
/// either way.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipam::store::Reservation;

    fn nat() -> Net {
        Net {
            title: "Lab".into(),
            mode: Mode::Nat,
            bridge: String::new(),
            autostart: true,
            gateway: "10.9.0.1".parse().unwrap(),
            prefix: 24,
            range_start: "10.9.0.2".parse().unwrap(),
            range_end: "10.9.0.254".parse().unwrap(),
            dns_forwarders: vec!["1.1.1.1".parse().unwrap(), "9.9.9.9".parse().unwrap()],
            domain: Some("lab".into()),
            lease_secs: 1800,
            reservations: vec![Reservation {
                mac: "52:54:00:aa:bb:01".into(),
                ip: "10.9.0.10".parse().unwrap(),
                name: Some("web".into()),
                dynamic: true,
                vm: Some("u1".into()),
                seen_ms: 7,
            }],
        }
    }

    const ID: &str = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";

    #[test]
    fn a_nat_network_round_trips_through_the_editor_xml() {
        let n = nat();
        let xml = render(ID, &n);
        assert!(xml.contains(&format!("<uuid>{ID}</uuid>")), "{xml}");
        assert!(
            xml.contains(&format!("<bridge name=\"{}\"/>", hostnet::bridge_name(ID))),
            "the derived bridge is shown, so the page's BRIDGE card is not empty:\n{xml}"
        );
        let back = parse(&xml).unwrap();
        assert_eq!(back.title, n.title);
        assert_eq!(
            (back.mode, back.gateway, back.prefix),
            (Mode::Nat, n.gateway, 24)
        );
        assert_eq!(
            (back.range_start, back.range_end),
            (n.range_start, n.range_end)
        );
        assert_eq!(back.domain, n.domain);
        assert_eq!(back.dns_forwarders, n.dns_forwarders);
        assert_eq!(back.reservations.len(), 1);
        assert_eq!(back.reservations[0].mac, n.reservations[0].mac);
        assert_eq!(back.reservations[0].name.as_deref(), Some("web"));
    }

    #[test]
    fn an_isolated_network_without_a_subnet_round_trips() {
        let n = Net {
            title: "Private".into(),
            mode: Mode::Isolated,
            ..nat()
        };
        let mut n = n;
        n.gateway = Ipv4Addr::UNSPECIFIED;
        n.prefix = 0;
        n.range_start = Ipv4Addr::UNSPECIFIED;
        n.range_end = Ipv4Addr::UNSPECIFIED;
        n.reservations.clear();
        n.dns_forwarders.clear();
        n.domain = None;
        let xml = render(ID, &n);
        assert!(xml.contains("<forward mode=\"none\"/>"), "{xml}");
        assert!(!xml.contains("<ip"), "no subnet, no <ip>:\n{xml}");
        let back = parse(&xml).unwrap();
        assert_eq!(back.mode, Mode::Isolated);
        assert!(back.gateway.is_unspecified() && !back.is_served());
        assert_eq!(back.title, "Private");
    }

    #[test]
    fn a_bridge_network_round_trips_with_its_bridge() {
        let mut n = nat();
        n.mode = Mode::Bridge;
        n.bridge = "br-lan".into();
        n.reservations.clear();
        let xml = render(ID, &n);
        assert!(xml.contains("<forward mode=\"bridge\"/>"), "{xml}");
        assert!(xml.contains("<bridge name=\"br-lan\"/>"), "{xml}");
        assert!(!xml.contains("<ip") && !xml.contains("<dns>"), "{xml}");
        let back = parse(&xml).unwrap();
        assert_eq!((back.mode, back.bridge.as_str()), (Mode::Bridge, "br-lan"));
        assert!(back.gateway.is_unspecified());
    }

    #[test]
    fn a_title_is_escaped_and_read_back_whole() {
        let mut n = nat();
        n.title = "R&D <lab> \"one\" & 'two'".into();
        let xml = render(ID, &n);
        assert!(!xml.contains("<lab>"), "the title is escaped:\n{xml}");
        assert_eq!(parse(&xml).unwrap().title, n.title);
    }

    #[test]
    fn a_range_that_is_not_a_usable_slice_of_the_subnet_is_refused() {
        // Outside the subnet.
        assert!(
            parse(
                "<network><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><range start='10.10.0.2' end='10.10.0.9'/></dhcp></ip></network>"
            )
            .is_err()
        );
        // Reversed.
        assert!(
            parse(
                "<network><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><range start='10.9.0.9' end='10.9.0.2'/></dhcp></ip></network>"
            )
            .is_err()
        );
        // Through the gateway.
        assert!(
            parse(
                "<network><ip address='10.9.0.5' netmask='255.255.255.0'>
              <dhcp><range start='10.9.0.2' end='10.9.0.9'/></dhcp></ip></network>"
            )
            .is_err(),
            "the gateway must not be handed out"
        );
        // A prefix dnsmasq and a bridge address cannot use.
        assert!(parse("<network><ip address='10.9.0.1' prefix='31'/></network>").is_err());
        assert!(parse("<network><ip address='10.9.0.1' prefix='0'/></network>").is_err());
    }

    #[test]
    fn an_unusable_bridge_or_host_is_refused() {
        for bridge in ["", "lo:1", "-x", "a name"] {
            let xml =
                format!("<network><forward mode='bridge'/><bridge name='{bridge}'/></network>");
            assert!(parse(&xml).is_err(), "{bridge:?} was accepted");
        }
        // A host outside the subnet, on a network with one.
        assert!(
            parse(
                "<network><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><host mac='52:54:00:aa:bb:01' ip='10.10.0.5'/></dhcp></ip></network>"
            )
            .is_err()
        );
        // A host name is cleaned, not refused: it usually comes from a guest.
        let cleaned = parse(
            "<network><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><host mac='52:54:00:AA:BB:01' ip='10.9.0.5' name='Bad Name'/></dhcp></ip></network>",
        )
        .unwrap();
        assert_eq!(cleaned.reservations[0].mac, "52:54:00:aa:bb:01");
        assert_eq!(cleaned.reservations[0].name.as_deref(), Some("bad-name"));
        // A host the gateway itself.
        assert!(
            parse(
                "<network><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><host mac='52:54:00:aa:bb:01' ip='10.9.0.1'/></dhcp></ip></network>"
            )
            .is_err()
        );
    }

    #[test]
    fn an_edit_keeps_autostart_the_lease_and_a_reservation_it_did_not_show() {
        let old = nat();
        let mut edited = parse(
            "<network><title>Edited</title><forward mode='nat'/><ip address='10.9.0.1' netmask='255.255.255.0'>
              <dhcp><range start='10.9.0.2' end='10.9.0.200'/>
                <host mac='52:54:00:aa:bb:01' ip='10.9.0.10' name='web'/></dhcp></ip></network>",
        )
        .unwrap();
        edited.reservations[0].dynamic = false;
        edited.reservations[0].vm = None;
        edited.reservations[0].seen_ms = 0;
        let merged = merge(&old, edited).unwrap();
        assert!(merged.autostart, "autostart is not in the editor's XML");
        assert_eq!(merged.lease_secs, 1800);
        assert_eq!(merged.title, "Edited");
        let r = &merged.reservations[0];
        assert!(
            r.dynamic && r.vm.as_deref() == Some("u1") && r.seen_ms == 7,
            "an unchanged reservation keeps what the editor cannot see"
        );
        // A reservation allocated after the form was opened is not dropped.
        let extra = merge(
            &old,
            parse("<network><forward mode='nat'/><ip address='10.9.0.1' netmask='255.255.255.0'/></network>")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(extra.reservations.len(), 1);
        assert_eq!(extra.reservations[0].mac, "52:54:00:aa:bb:01");

        // ...unless the subnet it lived in is gone: dnsmasq cannot serve a
        // dhcp-host outside its own subnet.
        let moved = merge(
            &old,
            parse("<network><forward mode='nat'/><ip address='10.44.0.1' netmask='255.255.255.0'/></network>")
                .unwrap(),
        )
        .unwrap();
        assert!(
            moved.reservations.is_empty(),
            "an address outside the new subnet is dropped: {:?}",
            moved.reservations
        );

        // A network that loses its subnet keeps no reservations at all.
        let stripped = merge(
            &old,
            parse("<network><forward mode='none'/></network>").unwrap(),
        )
        .unwrap();
        assert!(
            stripped.reservations.is_empty() && stripped.subnet().is_none(),
            "a subnetless network cannot hold an address: {:?}",
            stripped.reservations
        );
    }
}
