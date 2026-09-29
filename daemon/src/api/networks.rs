use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use virt::network::Network;

use super::ApiError;
use crate::ipam::server::Ipam;
use crate::ipam::xml as ipamxml;
use crate::libvirt::with_conn;

pub fn routes() -> Router {
    Router::new()
        .route("/networks", get(list))
        .route("/networks/bridges", get(bridges))
        .route("/networks/{name}", get(get_one).delete(delete))
        .route("/networks/{name}/start", post(start))
        .route("/networks/{name}/stop", post(stop))
        .route("/networks/{name}/autostart", post(set_autostart))
}

/// Routes that read or change IPAM state.
pub fn ipam_routes(ipam: Ipam) -> Router {
    Router::new()
        .route("/networks", axum::routing::post(create))
        .route("/networks/{name}/xml", get(get_xml).put(put_xml))
        .route("/networks/{name}/hosts", get(list_hosts).put(put_host))
        .route(
            "/networks/{name}/hosts/{mac}",
            axum::routing::delete(delete_host),
        )
        .route("/networks/{name}/ipam", get(get_ipam).put(put_ipam))
        .route("/flows", get(get_flows))
        .with_state(ipam)
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let nets = c.list_all_networks(0)?;
        let mut arr = Vec::with_capacity(nets.len());
        for n in &nets {
            arr.push(net_json(n));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn net_json(n: &Network) -> Value {
    // Forward mode and subnet live in the definition XML; the summary fields
    // otherwise cannot distinguish NAT from isolated, or show the subnet.
    let xml = n.get_xml_desc(0).unwrap_or_default();
    let forward = forward_mode(&xml).to_string();
    let ip = ip_cidr(&xml);
    json!({
        "name": n.get_name().unwrap_or_default(),
        "uuid": n.get_uuid_string().unwrap_or_default(),
        "title": crate::libvirt::title_of(&xml),
        "bridge": n.get_bridge_name().unwrap_or_default(),
        "active": n.is_active().unwrap_or(false),
        "persistent": n.is_persistent().unwrap_or(false),
        "autostart": n.get_autostart().unwrap_or(false),
        "forward": forward,
        "ip": ip,
    })
}

/// The network's forward mode: "nat", "bridge", or "none" (isolated).
fn forward_mode(xml: &str) -> &str {
    let Some(i) = xml.find("<forward") else {
        return "none";
    };
    let end = xml[i..].find('>').map_or(xml.len(), |j| i + j);
    let mode = attr_value(&xml[i..=end], "mode").unwrap_or("none");
    match mode {
        "bridge" => "bridge",
        "nat" | "route" => "nat",
        _ => "none",
    }
}

/// The first `<ip>` element's subnet, as `address/prefix` (or empty).
/// The first `<ip>` element's subnet, as `address/prefix` (or empty).
pub(crate) fn network_subnet(xml: &str) -> Option<String> {
    let c = ip_cidr(xml);
    if c.is_empty() { None } else { Some(c) }
}

fn ip_cidr(xml: &str) -> String {
    let Some(ip) = xml.find("<ip") else {
        return String::new();
    };
    let rest = &xml[ip..];
    let end = rest.find('>').unwrap_or(rest.len());
    let attrs = &rest[..=end];
    let address = attr_value(attrs, "address").unwrap_or_default();
    if address.is_empty() {
        return String::new();
    }
    let prefix = attr_value(attrs, "prefix")
        .map(|p| p.to_string())
        .or_else(|| attr_value(attrs, "netmask").map(|m| netmask_bits(m).to_string()))
        .unwrap_or_default();
    if prefix.is_empty() {
        address.to_string()
    } else {
        format!("{address}/{prefix}")
    }
}

/// Read one XML attribute's value out of a tag fragment, single- or
/// double-quoted. Matches only a real attribute position (name followed by
/// '='), never a substring inside another attribute's value.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(rel) = tag[from..].find(name) {
        let i = from + rel;
        from = i + name.len();
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' {
            continue;
        }
        let inner = &rest[quote.len_utf8()..];
        let j = inner.find(quote)?;
        return Some(&inner[..j]);
    }
    None
}

/// Dotted-quad netmask to prefix length (255.255.255.0 -> 24).
fn netmask_bits(mask: &str) -> u32 {
    mask.split('.')
        .filter_map(|o| o.parse::<u32>().ok())
        .map(|o| o.count_ones())
        .sum()
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = crate::libvirt::network(c, &name)?;
        Ok(net_json(&n))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Linux bridge devices, for the create/edit network form's bridge dropdown.
///
/// Read from /sys/class/net rather than libvirt interfaces: a bridge does not
/// have to be a libvirt interface, and scanning sysfs is what `ip link show
/// type bridge` does under the hood. A device is a bridge iff it has a
/// `bridge` subdirectory.
async fn bridges() -> Result<Json<Value>, ApiError> {
    let v = std::fs::read_dir("/sys/class/net")
        .map_err(|e| ApiError::internal(format!("read /sys/class/net: {e}")))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("bridge").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    Ok(Json(json!(v)))
}

/// The definition as the editor should see it: libvirt's, with this daemon's
/// IPAM (range, reservations, forwarders, domain) put back in, so the editor
/// works on one document whoever serves the network.
async fn get_xml(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let n2 = name.clone();
    let xml = with_conn(move |c| Ok(crate::libvirt::network(c, &n2)?.get_xml_desc(0)?))
        .await
        .map_err(ApiError::internal)?;
    let stored = ipam.lock().networks.get(&name).cloned();
    let xml = match stored {
        Some(net) if ipamxml::is_managed(&xml) => ipamxml::inject(&xml, &net),
        _ => xml,
    };
    Ok(Json(json!({"name": name, "xml": xml})))
}

/// Define/replace a network.
///
/// The IPAM part of the edited XML (DHCP range, reservations, DNS forwarders,
/// domain) goes to this daemon's store, and libvirt gets the definition
/// without it. If that is all that changed, the running network is left
/// alone: changing a range or a reservation never restarts anything. Only a
/// change to what libvirt itself runs (subnet, bridge, forward mode, a rename)
/// restarts the network, and its guests are plugged back in afterwards.
async fn put_xml(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let managed = ipamxml::needs_takeover(&body) || ipamxml::is_managed(&body);
    let libvirt_xml = if managed {
        ipamxml::strip(&body)
    } else {
        body.clone()
    };
    if managed {
        let merged = ipamxml::merge(ipam.lock().networks.get(&name), &body)
            .ok_or_else(|| ApiError::bad_request("the network has no IPv4 subnet"))?;
        let mut s = ipam.lock();
        s.networks.insert(name.clone(), merged);
        ipam.save(&s).map_err(ApiError::internal)?;
    }
    let old_name = name.clone();
    let v = with_conn(move |c| {
        let existing = crate::libvirt::network(c, &old_name).ok();
        let was_active = existing.as_ref().is_some_and(|n| n.is_active().unwrap_or(false));
        let before = existing
            .as_ref()
            .and_then(|n| n.get_xml_desc(virt::sys::VIR_NETWORK_XML_INACTIVE).ok())
            .unwrap_or_default();
        if same_definition(&before, &libvirt_xml) {
            return Ok(json!({"name": old_name, "defined": true, "restarted": false}));
        }
        if was_active && let Some(n) = &existing {
            n.destroy()?;
        }
        let n = Network::define_xml(c, &libvirt_xml)?;
        let new_name = n.get_name().unwrap_or_default();
        let (started, reattached) = if was_active {
            match n.create() {
                Ok(_) => (true, reattach_taps(c, &n)),
                Err(_) => (false, 0),
            }
        } else {
            (false, 0)
        };
        Ok(json!({"name": new_name, "defined": true, "restarted": started, "reattached": reattached}))
    })
    .await
    .map_err(ApiError::internal)?;
    // A rename moves the stored IPAM with it.
    if let Some(new) = v["name"].as_str().filter(|n| *n != name) {
        let mut s = ipam.lock();
        if let Some(net) = s.networks.remove(&name) {
            s.networks.insert(new.to_string(), net);
            ipam.save(&s).map_err(ApiError::internal)?;
        }
    }
    Ok(Json(v))
}

/// Whether two definitions describe the same network to libvirt, ignoring
/// what libvirt fills in by itself (uuid, generated MAC, stp/delay defaults)
/// and whitespace. Conservative: anything it cannot see as equal is a change.
fn same_definition(a: &str, b: &str) -> bool {
    let norm = |x: &str| {
        let mut s = ipamxml::strip_generated(x);
        s.retain(|c| !c.is_whitespace());
        s.replace('"', "'")
    };
    !a.is_empty() && norm(a) == norm(b)
}

/// Plug every running guest's interface on `n` back into its bridge.
///
/// Destroying a network deletes its bridge, and every guest tap on it is left
/// with no master; starting the network makes a new, empty bridge. libvirt does
/// not reconnect them (measured: the taps stayed detached, and update-device or
/// a link down/up did not help), so each guest silently lost its network until
/// it was restarted. That is what editing a network, or deleting an IPAM
/// reservation back when that redefined it, did to every guest on it.
///
/// Returns how many taps were reattached.
pub(crate) fn reattach_taps(c: &virt::connect::Connect, n: &Network) -> usize {
    let Ok(xml) = n.get_xml_desc(0) else { return 0 };
    let Some(bridge) = xml
        .find("<bridge")
        .and_then(|i| {
            attr_value(
                &xml[i..xml[i..].find('>').map_or(xml.len(), |e| i + e)],
                "name",
            )
        })
        .map(str::to_string)
    else {
        return 0;
    };
    let net = n.get_name().unwrap_or_default();
    let mut done = 0;
    for d in crate::libvirt::all_domains(c, virt::sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE) {
        let Ok(dx) = d.get_xml_desc(0) else { continue };
        for tap in taps_on_network(&dx, &net) {
            let ok = std::process::Command::new("ip")
                .args(["link", "set", "dev", &tap, "master", &bridge])
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                done += 1;
            } else {
                tracing::warn!("could not reattach {tap} to {bridge}");
            }
        }
    }
    done
}

/// Target devs of a live domain's interfaces on network `net`.
fn taps_on_network(domain_xml: &str, net: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = domain_xml;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let Some(end) = block.find("</interface>") else {
            break;
        };
        let b = &block[..end];
        let on_net = b.find("<source").is_some_and(|s| {
            let tag = &b[s..b[s..].find('>').map_or(b.len(), |e| s + e)];
            attr_value(tag, "network") == Some(net)
        });
        if on_net
            && let Some(t) = b.find("<target")
            && let Some(dev) = attr_value(&b[t..b[t..].find('>').map_or(b.len(), |e| t + e)], "dev")
        {
            out.push(dev.to_string());
        }
        rest = &block[end + 12..];
    }
    out
}

async fn create(State(ipam): State<Ipam>, body: String) -> Result<Json<Value>, ApiError> {
    // A new network with an IPv4 subnet is served by this daemon from the
    // start, so libvirt never runs dnsmasq for it.
    let libvirt_xml = if ipamxml::needs_takeover(&body) {
        let net = crate::ipam::store::import(&body)
            .ok_or_else(|| ApiError::bad_request("the network has no IPv4 subnet"))?;
        let name =
            open_name(&body).ok_or_else(|| ApiError::bad_request("the network has no name"))?;
        let mut s = ipam.lock();
        s.networks.insert(name, net);
        ipam.save(&s).map_err(ApiError::internal)?;
        ipamxml::strip(&body)
    } else {
        body
    };
    let v = with_conn(move |c| {
        let n = Network::define_xml(c, &libvirt_xml)?;
        let name = n.get_name().unwrap_or_default();
        n.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = crate::libvirt::network(c, &name)?;
        if n.is_active().unwrap_or(false) {
            n.destroy()?;
        }
        n.undefine()?;
        Ok(json!({"name": name, "undefined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = crate::libvirt::network(c, &name)?;
        n.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn stop(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = crate::libvirt::network(c, &name)?;
        n.destroy()?;
        Ok(json!({"name": name, "stopped": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct AutostartReq {
    autostart: bool,
}

async fn set_autostart(
    Path(name): Path<String>,
    Json(req): Json<AutostartReq>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = crate::libvirt::network(c, &name)?;
        n.set_autostart(req.autostart)?;
        Ok(json!({"name": name, "autostart": req.autostart}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn open_name(xml: &str) -> Option<String> {
    let i = xml.find("<name>")? + 6;
    let j = xml[i..].find("</name>")?;
    Some(xml[i..i + j].trim().to_string())
}

fn managed(ipam: &Ipam, name: &str) -> Result<crate::ipam::store::Net, ApiError> {
    ipam.lock().networks.get(name).cloned().ok_or_else(|| {
        ApiError::bad_request(format!(
            "network '{name}' has no IPv4 subnet served by this host, so it has no address reservations"
        ))
    })
}

fn reservation_json(r: &crate::ipam::store::Reservation, guests: &Ipam) -> Value {
    json!({
        "mac": r.mac,
        "ip": r.ip.to_string(),
        "name": r.name,
        "dynamic": r.dynamic,
        "vm": r.vm,
        "vm_name": guests.guest_of(&r.mac).map(|g| g.1),
        "seen_ms": r.seen_ms,
    })
}

/// Every reservation on a network, with the guest each belongs to.
async fn list_hosts(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let net = managed(&ipam, &name)?;
    let hosts: Vec<Value> = net
        .reservations
        .iter()
        .map(|r| reservation_json(r, &ipam))
        .collect();
    Ok(Json(json!({"network": name, "hosts": hosts})))
}

/// A DHCP reservation, as the VM Network tab edits it.
#[derive(Deserialize)]
struct HostReq {
    mac: String,
    ip: String,
    #[serde(default)]
    name: Option<String>,
}

/// Add or change the reservation for one MAC. Takes effect at the guest's
/// next DHCP exchange; nothing is restarted.
async fn put_host(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
    Json(req): Json<HostReq>,
) -> Result<Json<Value>, ApiError> {
    use crate::ipam::store::{Reservation, clean_hostname, normalize_mac};
    let mac = normalize_mac(&req.mac)
        .ok_or_else(|| ApiError::bad_request(format!("'{}' is not a MAC address", req.mac)))?;
    let ip: std::net::Ipv4Addr = req
        .ip
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request(format!("'{}' is not an IPv4 address", req.ip)))?;
    let host = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(h) => Some(
            clean_hostname(h)
                .filter(|c| c == &h.to_ascii_lowercase())
                .ok_or_else(|| {
                    ApiError::bad_request("a hostname may only contain letters, digits and '-'")
                })?,
        ),
        None => None,
    };
    managed(&ipam, &name)?;
    let vm = ipam.guest_of(&mac).map(|g| g.0);
    let r = ipam
        .update(&name, |n| {
            if !n.contains_host(ip) || ip == n.gateway {
                return Err(format!("{ip} is not a usable address on this network"));
            }
            if let Some(other) = n.reservations.iter().find(|r| r.ip == ip && r.mac != mac) {
                return Err(format!("{ip} is already reserved for {}", other.mac));
            }
            let seen = n.reservation_for_mac(&mac).map_or(0, |r| r.seen_ms);
            n.reservations.retain(|r| r.mac != mac);
            let r = Reservation {
                mac: mac.clone(),
                ip,
                name: host,
                dynamic: false,
                vm,
                seen_ms: seen,
            };
            n.reservations.push(r.clone());
            Ok(r)
        })
        .map_err(ApiError::bad_request)?;
    Ok(Json(
        json!({"network": name, "host": reservation_json(&r, &ipam)}),
    ))
}

/// Drop the reservation for one MAC.
///
/// The address is free again, but the guest is not left without one: its next
/// DHCP exchange gets a fresh address from the range, which becomes its
/// permanent reservation in turn. (Deleting used to redefine the network and
/// unplug every guest on it; now nothing is restarted.)
async fn delete_host(
    State(ipam): State<Ipam>,
    Path((name, mac)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let mac = crate::ipam::store::normalize_mac(&mac)
        .ok_or_else(|| ApiError::bad_request("not a MAC address"))?;
    managed(&ipam, &name)?;
    let removed = ipam
        .update(&name, |n| {
            let before = n.reservations.len();
            n.reservations.retain(|r| r.mac != mac);
            Ok(n.reservations.len() != before)
        })
        .map_err(ApiError::bad_request)?;
    Ok(Json(
        json!({"network": name, "mac": mac, "removed": removed}),
    ))
}

#[derive(Deserialize)]
struct FlowsQuery {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    limit: Option<usize>,
}

/// Guest connections recorded since `after` (a sequence number), oldest
/// first. The web server pulls these into its access log. `last` is the
/// newest sequence here; `dropped` counts records that aged out of the ring
/// before anyone pulled them.
async fn get_flows(
    State(ipam): State<Ipam>,
    axum::extract::Query(q): axum::extract::Query<FlowsQuery>,
) -> Json<Value> {
    let (flows, last, dropped) = ipam
        .flows
        .since(q.after, q.limit.unwrap_or(5000).min(20_000));
    Json(json!({"flows": flows, "last": last, "dropped": dropped}))
}

/// The network's IPAM settings, and whether this host is serving it now.
async fn get_ipam(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let n = managed(&ipam, &name)?;
    let (a, b) = (u32::from(n.range_start), u32::from(n.range_end));
    let used = n
        .reservations
        .iter()
        .filter(|r| (a..=b).contains(&u32::from(r.ip)))
        .count() as u32;
    Ok(Json(json!({
        "network": name,
        "serving": ipam.serving(&name),
        "gateway": n.gateway.to_string(),
        "prefix": n.prefix,
        "range_start": n.range_start.to_string(),
        "range_end": n.range_end.to_string(),
        "range_size": b.saturating_sub(a) + 1,
        "range_used": used,
        "dns_forwarders": n.dns_forwarders.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "domain": n.domain,
        "lease_secs": n.lease_secs,
    })))
}

#[derive(Deserialize)]
struct IpamReq {
    range_start: String,
    range_end: String,
    #[serde(default)]
    dns_forwarders: Vec<String>,
    #[serde(default)]
    domain: Option<String>,
}

/// Change the dynamic range, forwarders or domain. Existing reservations are
/// kept even when the range no longer covers them: they are permanent.
async fn put_ipam(
    State(ipam): State<Ipam>,
    Path(name): Path<String>,
    Json(req): Json<IpamReq>,
) -> Result<Json<Value>, ApiError> {
    let parse = |s: &str| -> Result<std::net::Ipv4Addr, ApiError> {
        s.trim()
            .parse()
            .map_err(|_| ApiError::bad_request(format!("'{s}' is not an IPv4 address")))
    };
    let (start, end) = (parse(&req.range_start)?, parse(&req.range_end)?);
    let fwd = req
        .dns_forwarders
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| parse(s))
        .collect::<Result<Vec<_>, _>>()?;
    let domain = match req
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(d)
            if d.split('.').all(|l| {
                crate::ipam::store::clean_hostname(l).as_deref() == Some(&l.to_ascii_lowercase())
            }) =>
        {
            Some(d.to_ascii_lowercase())
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "the domain may only contain letters, digits, '-' and '.'",
            ));
        }
        None => None,
    };
    managed(&ipam, &name)?;
    ipam.update(&name, |n| {
        n.validate_range(start, end)?;
        n.range_start = start;
        n.range_end = end;
        n.dns_forwarders = fwd;
        n.domain = domain;
        Ok(())
    })
    .map_err(ApiError::bad_request)?;
    get_ipam(State(ipam), Path(name)).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn taps_are_found_only_for_the_restarted_network() {
        let xml = "<domain><devices>
          <interface type='network'><source network='internet' bridge='virbr1'/><target dev='vnet1'/></interface>
          <interface type='network'><source network='internal'/><target dev='vnet2'/></interface>
          <interface type=\"network\"><mac address='x'/><source network=\"internet\"/><target dev=\"vnet9\"/></interface>
          <interface type='network'><source network='internet'/></interface>
        </devices></domain>";
        assert_eq!(
            super::taps_on_network(xml, "internet"),
            vec!["vnet1", "vnet9"]
        );
        assert_eq!(super::taps_on_network(xml, "internal"), vec!["vnet2"]);
        assert!(super::taps_on_network(xml, "other").is_empty());
    }

    use super::*;

    #[test]
    fn forward_mode_reads_nat_bridge_and_none() {
        // libvirt emits single-quoted attributes.
        assert_eq!(
            forward_mode(r#"<network><forward mode='nat'/></network>"#),
            "nat"
        );
        assert_eq!(
            forward_mode(r#"<network><forward mode='bridge'/></network>"#),
            "bridge"
        );
        // No forward element = isolated.
        assert_eq!(forward_mode(r#"<network><name>x</name></network>"#), "none");
        // route behaves like nat for display purposes.
        assert_eq!(
            forward_mode(r#"<network><forward mode='route'/></network>"#),
            "nat"
        );
    }

    #[test]
    fn ip_cidr_reads_address_and_netmask_as_prefix() {
        assert_eq!(
            ip_cidr(r#"<network><ip address='192.168.100.1' netmask='255.255.255.0'/></network>"#),
            "192.168.100.1/24"
        );
        // Double quotes work too.
        assert_eq!(
            ip_cidr(r#"<network><ip address="10.9.0.1" netmask="255.255.0.0"/></network>"#),
            "10.9.0.1/16"
        );
        assert_eq!(ip_cidr(r#"<network><name>x</name></network>"#), "");
    }

    #[test]
    fn ip_cidr_prefers_an_explicit_prefix() {
        assert_eq!(
            ip_cidr(r#"<network><ip address='10.0.0.1' prefix='16'/></network>"#),
            "10.0.0.1/16"
        );
    }

    #[test]
    fn netmask_bits_sums_leading_ones() {
        assert_eq!(netmask_bits("255.255.255.0"), 24);
        assert_eq!(netmask_bits("255.255.0.0"), 16);
        assert_eq!(netmask_bits("255.255.255.128"), 25);
    }
}
