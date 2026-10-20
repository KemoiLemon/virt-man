//! The network API: the store is the source of truth, `hostnet` runs the
//! bridge and dnsmasq, and the firewall table follows every lifecycle change.
//!
//! The SPA still edits a network as libvirt-shaped XML (`netxml`), but nothing
//! here delegates to another network manager: the store is the only definition.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use super::firewall::FirewallState;
use crate::firewall;
use crate::hostnet;
use crate::ipam::server::Ipam;
use crate::ipam::store::{self, Mode, Net, Reservation};
use crate::netxml;

#[derive(Clone)]
struct NetState {
    ipam: Ipam,
    fw: FirewallState,
}

pub fn routes(ipam: Ipam, fw: FirewallState) -> Router {
    Router::new()
        .route("/networks", get(list).post(create))
        .route("/networks/bridges", get(bridges))
        .route("/networks/suggest-subnet", get(suggest_subnet))
        .route("/networks/{name}", get(get_one).delete(delete))
        .route("/networks/{name}/xml", get(get_xml).put(put_xml))
        .route("/networks/{name}/start", post(start))
        .route("/networks/{name}/stop", post(stop))
        .route("/networks/{name}/autostart", post(set_autostart))
        .route("/networks/{name}/hosts", get(list_hosts).put(put_host))
        .route(
            "/networks/{name}/hosts/{mac}",
            axum::routing::delete(delete_host),
        )
        .route("/networks/{name}/ipam", get(get_ipam).put(put_ipam))
        .route("/flows", get(get_flows))
        .with_state(NetState { ipam, fw })
}

/// Whether `s` is a UUID in canonical 8-4-4-4-12 form, so a network's name can
/// be one and an arbitrary string is refused.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The host's own interfaces and routes, as `(network address, prefix)`.
///
/// Read from `ip -o addr` (the host's real LAN) and `ip -o route` (a VPN on
/// 10.0.0.0/8 has no interface of its own, so the route table is the only
/// place it shows). A generated /24 must collide with neither.
fn used_prefixes_host() -> Vec<(u32, u8)> {
    let mut out: Vec<(u32, u8)> = Vec::new();
    // The host's own addressing and routes. `ip -o addr` and `ip -o route` are
    // the two commands that show both without guessing at /proc formats.
    for args in [["-o", "addr", "show"], ["-o", "route", "show"]] {
        let Ok(o) = std::process::Command::new("ip").args(args).output() else {
            continue;
        };
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            for tok in line.split_whitespace() {
                // `10.0.0.5/24` in addr output, `10.0.0.0/24` in route output.
                if let Some((net, p)) = read_cidr(tok) {
                    out.push((net, p));
                }
            }
        }
    }
    out
}

/// Every IPv4 prefix this host can already reach: its interfaces and routes,
/// plus every stored network except `except` (the one being edited, which owns
/// its own prefix).
///
/// A generated /24 must collide with none of them. The stored half matters
/// even for a network that is down now: it still owns its address, and reusing
/// it would break it when it starts.
async fn used_prefixes(ipam: &Ipam, except: Option<&str>) -> Vec<(u32, u8)> {
    let mut out = tokio::task::spawn_blocking(used_prefixes_host)
        .await
        .unwrap_or_default();
    let nets: Vec<(String, Net)> = ipam
        .lock()
        .networks
        .iter()
        .map(|(id, n)| (id.clone(), n.clone()))
        .collect();
    for (id, n) in nets {
        if Some(id.as_str()) == except {
            continue;
        }
        if let Some(found) = n.subnet().as_deref().and_then(read_cidr) {
            out.push(found);
        }
    }
    out
}

/// `10.0.0.0/24` or a bare address to `(network address, prefix)`. Accepts
/// both, since addr output carries a host address and route output a network.
fn read_cidr(tok: &str) -> Option<(u32, u8)> {
    let (addr, prefix) = match tok.split_once('/') {
        Some((a, p)) => (a, p.parse::<u8>().ok()?.min(32)),
        None => (tok, 32),
    };
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    Some((u32::from(ip) & store::mask(prefix), prefix))
}

/// A free `/24` for a new network, so a user never picks an address that
/// collides. Read-only: the SPA asks for this to prefill the form, and the
/// create path checks conflicts itself again, since a caller may skip this.
async fn suggest_subnet(State(st): State<NetState>) -> Result<Json<Value>, ApiError> {
    let used = used_prefixes(&st.ipam, None).await;
    let subnet = free_subnet(&used)?;
    // The **gateway**, not the network address: x.x.x.0 is the subnet's own
    // address and cannot be a host, so the value a form puts in its <ip
    // address> must be x.x.x.1. `pick_subnet` works in network addresses
    // because that is what an overlap check needs, so the +1 happens here.
    let gateway = std::net::Ipv4Addr::from(u32::from(subnet) + 1);
    Ok(Json(json!({
        "subnet": format!("{gateway}/24"),
        "network": format!("{subnet}/24"),
        "used": used.len(),
    })))
}

/// A free `/24`, or an error naming what to do about it.
fn free_subnet(used: &[(u32, u8)]) -> Result<std::net::Ipv4Addr, ApiError> {
    let mut r = 0u64;
    let mut rand = || {
        if r == 0 {
            let mut b = [0u8; 8];
            r = if std::fs::File::open("/dev/urandom")
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
                .is_ok()
            {
                u64::from_le_bytes(b)
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(1)
                    ^ (std::process::id() as u64) << 32
            };
        }
        let v = r;
        r = r.rotate_left(17);
        v as u32
    };
    store::pick_subnet(used, &mut rand).ok_or_else(|| {
        ApiError::internal(
            "no free /24 left in 10/8 or 192.168/16: free an address range by deleting a network",
        )
    })
}

/// Give a NAT network the free /24 it must route and hand out addresses from.
/// A bridge network's addressing is the operator's, and an isolated segment may
/// deliberately have none, so neither is given one.
async fn with_nat_subnet(ipam: &Ipam, mut net: Net) -> Result<Net, ApiError> {
    if net.mode == Mode::Nat && net.subnet().is_none() {
        let used = used_prefixes(ipam, None).await;
        let subnet = free_subnet(&used)?;
        net.gateway = std::net::Ipv4Addr::from(u32::from(subnet) + 1);
        net.prefix = 24;
        net.default_range();
    }
    Ok(net)
}

/// The host's own prefixes, gathered once per write before the store lock.
/// `omit` drops the edited network's own bridge address: it is on the host, but
/// it is about to be replaced, so it must not read as a collision with itself.
async fn host_prefixes(omit: Option<(u32, u8)>) -> Vec<(u32, u8)> {
    let mut out = tokio::task::spawn_blocking(used_prefixes_host)
        .await
        .unwrap_or_default();
    if let Some(omit) = omit {
        out.retain(|p| *p != omit);
    }
    out
}

/// Refuse a subnet (supplied in the editor, not generated) that overlaps the
/// host's own addressing or another stored network's. Called under the store
/// lock, so two creates cannot both pass and then claim one range.
fn subnet_clash(
    s: &store::Store,
    host: &[(u32, u8)],
    net: &Net,
    except: Option<&str>,
) -> Result<(), ApiError> {
    if net.mode == Mode::Bridge {
        return Ok(());
    }
    let Some(subnet) = net.subnet() else {
        return Ok(());
    };
    let Some((network, prefix)) = read_cidr(&subnet) else {
        return Ok(());
    };
    let mut used = host.to_vec();
    for (id, n) in &s.networks {
        if Some(id.as_str()) == except {
            continue;
        }
        if let Some(found) = n.subnet().as_deref().and_then(read_cidr) {
            used.push(found);
        }
    }
    if store::overlaps_any(network, prefix, &used) {
        return Err(ApiError::bad_request(format!(
            "the subnet {subnet} overlaps an address range already in use (the host's interfaces and routes, or another network); pick an unused one"
        )));
    }
    Ok(())
}

/// A network's display name must resolve to exactly one network: a title equal
/// to another network's key or title would make `key()` ambiguous.
fn check_title(s: &store::Store, id: &str, title: &str) -> Result<(), ApiError> {
    if title.is_empty() {
        return Ok(());
    }
    let clash = s
        .networks
        .iter()
        .any(|(other, n)| other != id && (other == title || n.title == title));
    if clash {
        return Err(ApiError::bad_request(format!(
            "another network is already called '{title}'"
        )));
    }
    Ok(())
}

/// Two networks must never derive one bridge name: each Start would flush the
/// other's address, and one delete would take the other's bridge with it. Two
/// *operator* bridges may deliberately share a LAN, so that pair is allowed —
/// but naming a managed network's derived bridge is never.
fn check_bridge(s: &store::Store, id: &str, net: &Net) -> Result<(), ApiError> {
    let bridge = hostnet::bridge_for(net, id);
    if bridge.is_empty() {
        return Ok(());
    }
    let clash = s.networks.iter().find(|(other, n)| {
        other.as_str() != id
            && hostnet::bridge_for(n, other) == bridge
            && !(net.mode == Mode::Bridge && n.mode == Mode::Bridge)
    });
    if let Some((other, _)) = clash {
        return Err(ApiError::conflict(format!(
            "network '{other}' already uses the bridge '{bridge}'"
        )));
    }
    Ok(())
}

fn net_json(id: &str, net: &Net) -> Value {
    json!({
        "name": id,
        "uuid": id,
        "title": net.title,
        "bridge": hostnet::bridge_for(net, id),
        "active": hostnet::is_active(net, id),
        // A stored network is persistent by definition: nothing removes one
        // but the API's delete.
        "persistent": true,
        "autostart": net.autostart,
        "forward": match net.mode {
            Mode::Nat => "nat",
            Mode::Bridge => "bridge",
            Mode::Isolated => "none",
        },
        "ip": net.cidr().unwrap_or_default(),
    })
}

fn net_of(ipam: &Ipam, name: &str) -> Result<Net, ApiError> {
    ipam.lock()
        .networks
        .get(name)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("network '{name}' is not managed here")))
}

/// The stored key a path segment names: the key itself (a UUID), or the title
/// it displays, for an old link by name. Titles are not unique, so the first
/// in key order wins. An unknown segment is returned unchanged, so the handler
/// answers "not found" exactly as it would for a wrong id.
///
/// This lives here rather than in `resolve_uuids` because a router captures
/// `Path` params before any per-route middleware runs, so rewriting the URI
/// there could not change what a handler reads.
fn key(ipam: &Ipam, seg: &str) -> String {
    ipam.resolve_key(seg).unwrap_or_else(|| seg.to_string())
}

async fn list(State(st): State<NetState>) -> Result<Json<Value>, ApiError> {
    let nets: Vec<(String, Net)> = st
        .ipam
        .lock()
        .networks
        .iter()
        .map(|(id, n)| (id.clone(), n.clone()))
        .collect();
    let arr: Vec<Value> = nets.iter().map(|(id, n)| net_json(id, n)).collect();
    Ok(Json(json!(arr)))
}

async fn get_one(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = net_of(&st.ipam, &name)?;
    Ok(Json(net_json(&name, &net)))
}

/// Linux bridge devices, for the create/edit network form's bridge dropdown.
///
/// Read from /sys/class/net rather than a manager's interface list: any host
/// bridge qualifies, and scanning sysfs is what `ip link show type bridge`
/// does under the hood. A device is a bridge iff it has a `bridge`
/// subdirectory.
async fn bridges() -> Result<Json<Value>, ApiError> {
    let v = std::fs::read_dir("/sys/class/net")
        .map_err(|e| ApiError::internal(format!("read /sys/class/net: {e}")))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("bridge").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    Ok(Json(json!(v)))
}

async fn get_xml(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = net_of(&st.ipam, &name)?;
    Ok(Json(
        json!({"name": name, "xml": netxml::render(&name, &net)}),
    ))
}

/// Create a network. The SPA names it with a fresh UUID and puts the name the
/// user typed in `<title>`, so the id and the display name cannot drift.
async fn create(State(st): State<NetState>, body: String) -> Result<Json<Value>, ApiError> {
    let name = open_name(&body).ok_or_else(|| ApiError::bad_request("the network has no name"))?;
    if !is_uuid(&name) {
        return Err(ApiError::bad_request(format!(
            "network '{name}' must be named by a UUID"
        )));
    }
    let net = with_nat_subnet(
        &st.ipam,
        netxml::parse(&body).map_err(ApiError::bad_request)?,
    )
    .await?;
    let host = host_prefixes(None).await;
    {
        let mut s = st.ipam.lock();
        if s.networks.contains_key(&name) {
            return Err(ApiError::conflict(format!(
                "network '{name}' already exists"
            )));
        }
        check_title(&s, &name, &net.title)?;
        check_bridge(&s, &name, &net)?;
        subnet_clash(&s, &host, &net, None)?;
        s.networks.insert(name.clone(), net.clone());
        st.ipam.save(&s).map_err(ApiError::internal)?;
    }
    let macs = st.ipam.dhcp_macs(&net, &name);
    if let Err(e) = st
        .ipam
        .host
        .start(&net, &name, &net.reservations, &macs)
        .await
    {
        // Roll back what the failed start may already have made: dnsmasq, its
        // files, the bridge. Then forget the definition, rather than leave a
        // broken network every later startup retries.
        let _ = st.ipam.host.stop(&net, &name).await;
        st.ipam.host.remove(&name);
        {
            let mut s = st.ipam.lock();
            s.networks.remove(&name);
            let _ = st.ipam.save(&s);
        }
        return Err(ApiError::internal(e));
    }
    apply_firewall(&st).await?;
    Ok(Json(json!({"name": name, "started": true})))
}

/// Define/replace a network from the editor's XML.
///
/// Only a change to what runs (bridge, subnet, mode) reprograms the bridge; an
/// edit to the range, forwarders, domain or reservations just restarts dnsmasq,
/// so a guest is never unplugged for a settings change. A bridge *rename* stops
/// the old one first; a same-bridge subnet change is applied in place.
/// `hostnet::bridge_up`, not `is_active`, decides "was up": a crashed dnsmasq
/// must not stop the address from being reprogrammed.
async fn put_xml(
    State(st): State<NetState>,
    Path(name): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    if open_name(&body).as_deref() != Some(name.as_str()) {
        return Err(ApiError::bad_request(format!(
            "the definition's <name> must be this network's id '{name}'"
        )));
    }
    let old = net_of(&st.ipam, &name)?;
    let new = netxml::parse(&body).map_err(ApiError::bad_request)?;
    let new = with_nat_subnet(&st.ipam, new).await?;
    let net = netxml::merge(&old, new).map_err(ApiError::bad_request)?;
    let old_bridge = hostnet::bridge_for(&old, &name);
    let new_bridge = hostnet::bridge_for(&net, &name);
    // `host.stop` deletes a managed bridge, and with it the address and every
    // guest tap. Only a *different* bridge needs that: when the name stays,
    // `host.start` reprograms the address and restarts dnsmasq in place, so a
    // subnet change does not pull the network out from under its guests.
    let same_bridge = old_bridge == new_bridge;
    let reprogram = !same_bridge || old.cidr() != net.cidr() || old.mode != net.mode;
    let dns_changed = old.range_start != net.range_start
        || old.range_end != net.range_end
        || old.dns_forwarders != net.dns_forwarders
        || old.domain != net.domain
        || old.lease_secs != net.lease_secs
        || old.reservations != net.reservations;
    // `bridge_up`, not `is_active`: a crashed dnsmasq must not decide whether
    // the address is reprogrammed.
    let was_up = hostnet::bridge_up(&old, &name);
    // The old bridge's address is on the host, but it is this network's own.
    let own = old.subnet().as_deref().and_then(read_cidr);
    let host = host_prefixes(own).await;
    // Refuse before stopping anything: a rejected definition must leave the
    // running network exactly as it is.
    {
        let s = st.ipam.lock();
        check_title(&s, &name, &net.title)?;
        check_bridge(&s, &name, &net)?;
        subnet_clash(&s, &host, &net, Some(&name))?;
    }
    if reprogram && was_up && !same_bridge {
        st.ipam
            .host
            .stop(&old, &name)
            .await
            .map_err(ApiError::internal)?;
    }
    {
        let mut s = st.ipam.lock();
        s.networks.insert(name.clone(), net.clone());
        st.ipam.save(&s).map_err(ApiError::internal)?;
    }
    if reprogram {
        if was_up {
            let macs = st.ipam.dhcp_macs(&net, &name);
            if let Err(e) = st
                .ipam
                .host
                .start(&net, &name, &net.reservations, &macs)
                .await
            {
                // The new definition never came up. Put the old one back so a
                // reload shows what is actually stored, restore what runs, and
                // rebuild the rules for it.
                tracing::warn!(
                    "ipam: network {name} did not start: {e}; restoring the old definition"
                );
                let _ = st.ipam.host.stop(&net, &name).await;
                let macs = st.ipam.dhcp_macs(&old, &name);
                let _ = st
                    .ipam
                    .host
                    .start(&old, &name, &old.reservations, &macs)
                    .await;
                {
                    let mut s = st.ipam.lock();
                    s.networks.insert(name.clone(), old.clone());
                    let _ = st.ipam.save(&s);
                }
                apply_firewall(&st).await?;
                return Err(ApiError::internal(e));
            }
        }
        // A network that becomes the operator's must not keep a dnsmasq of
        // ours on that bridge.
        if net.mode == Mode::Bridge && old.mode != Mode::Bridge {
            st.ipam
                .host
                .stop_dnsmasq(&name)
                .await
                .map_err(ApiError::internal)?;
        }
        apply_firewall(&st).await?;
    } else if dns_changed {
        restart_dns(&st, &name).await?;
    }
    Ok(Json(
        json!({"name": name, "defined": true, "restarted": reprogram && was_up}),
    ))
}

async fn start(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = net_of(&st.ipam, &name)?;
    let macs = st.ipam.dhcp_macs(&net, &name);
    st.ipam
        .host
        .start(&net, &name, &net.reservations, &macs)
        .await
        .map_err(ApiError::internal)?;
    apply_firewall(&st).await?;
    Ok(Json(json!({"name": name, "started": true})))
}

async fn stop(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = net_of(&st.ipam, &name)?;
    st.ipam
        .host
        .stop(&net, &name)
        .await
        .map_err(ApiError::internal)?;
    apply_firewall(&st).await?;
    Ok(Json(json!({"name": name, "stopped": true})))
}

#[derive(Deserialize)]
struct AutostartReq {
    autostart: bool,
}

/// Autostart lives in the store only: it changes what the next startup does,
/// not what runs now.
async fn set_autostart(
    State(st): State<NetState>,
    Path(name): Path<String>,
    Json(req): Json<AutostartReq>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    net_of(&st.ipam, &name)?;
    st.ipam
        .update(&name, |n| {
            n.autostart = req.autostart;
            Ok(())
        })
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"name": name, "autostart": req.autostart})))
}

async fn delete(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = net_of(&st.ipam, &name)?;
    // Deleting takes the bridge away under its guests (and their DHCP/DNS).
    // Refuse while any *defined* guest names it: a shut-off guest's NIC still
    // points at the bridge, and it could not start after the bridge is gone.
    let bridge = hostnet::bridge_for(&net, &name);
    let mut users: Vec<String> = st
        .ipam
        .nics()
        .into_iter()
        .filter(|n| n.bridge == bridge)
        .map(|n| n.name)
        .collect();
    if !users.is_empty() {
        users.sort();
        users.dedup();
        return Err(ApiError::conflict(format!(
            "network '{name}' is in use by defined guest(s) {}; delete or move them first",
            users.join(", ")
        )));
    }
    st.ipam
        .host
        .stop(&net, &name)
        .await
        .map_err(ApiError::internal)?;
    st.ipam.host.remove(&name);
    {
        let mut s = st.ipam.lock();
        s.networks.remove(&name);
        st.ipam.save(&s).map_err(ApiError::internal)?;
    }
    // The firewall entry, its file and the live ruleset in one critical
    // section: rules for a network that no longer exists must not survive, and
    // a new network reusing the id must not inherit them.
    let _guard = st.fw.lock.lock().await;
    let mut store = firewall::load(&st.fw.path).map_err(ApiError::internal)?;
    if store.networks.remove(&name).is_some() {
        firewall::save(&st.fw.path, &store).map_err(ApiError::internal)?;
    }
    crate::api::firewall::apply_store(&st.ipam, &store)
        .await
        .map_err(|e| {
            ApiError::internal(format!(
                "network removed, applying the firewall failed: {e}"
            ))
        })?;
    Ok(Json(json!({"name": name, "undefined": true})))
}

/// Rebuild the `virt-man` nftables table from the persisted store. Serialised
/// with the firewall API's own saves so a concurrent PUT cannot interleave the
/// file and the live ruleset.
async fn apply_firewall(st: &NetState) -> Result<(), ApiError> {
    let _guard = st.fw.lock.lock().await;
    let store = firewall::load(&st.fw.path).map_err(ApiError::internal)?;
    crate::api::firewall::apply_store(&st.ipam, &store)
        .await
        .map_err(|e| {
            ApiError::internal(format!(
                "network change applied, applying the firewall failed: {e}"
            ))
        })
}

/// Rewrite a network's dnsmasq config and restart it, if it is running. That is
/// what makes a changed reservation or range take effect without touching the
/// bridge (and so without unplugging a guest).
async fn restart_dns(st: &NetState, id: &str) -> Result<(), ApiError> {
    let Some(net) = st.ipam.lock().networks.get(id).cloned() else {
        return Ok(());
    };
    if !net.is_served() || !hostnet::is_active(&net, id) {
        return Ok(());
    }
    let macs = st.ipam.dhcp_macs(&net, id);
    st.ipam
        .host
        .restart_dnsmasq(&net, id, &net.reservations, &macs)
        .await
        .map_err(ApiError::internal)
}

/// A network whose addressing this host serves. A bridge network, or one with
/// no subnet, has no range or reservations to edit.
fn managed(ipam: &Ipam, name: &str) -> Result<Net, ApiError> {
    let err = || {
        ApiError::bad_request(format!(
            "network '{name}' has no IPv4 subnet served by this host, so it has no address reservations"
        ))
    };
    let net = ipam.lock().networks.get(name).cloned().ok_or_else(err)?;
    if !net.is_served() {
        return Err(err());
    }
    Ok(net)
}

fn reservation_json(r: &Reservation, ipam: &Ipam) -> Value {
    json!({
        "mac": r.mac,
        "ip": r.ip.to_string(),
        "name": r.name,
        "dynamic": r.dynamic,
        "vm": r.vm,
        "vm_name": ipam.guest_of(&r.mac).map(|g| g.name),
        "seen_ms": r.seen_ms,
    })
}

/// Every reservation on a network, with the guest each belongs to.
async fn list_hosts(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let net = managed(&st.ipam, &name)?;
    let hosts: Vec<Value> = net
        .reservations
        .iter()
        .map(|r| reservation_json(r, &st.ipam))
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

/// Add or change the reservation for one MAC.
async fn put_host(
    State(st): State<NetState>,
    Path(name): Path<String>,
    Json(req): Json<HostReq>,
) -> Result<Json<Value>, ApiError> {
    use crate::ipam::store::{clean_hostname, normalize_mac};
    let name = key(&st.ipam, &name);
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
    managed(&st.ipam, &name)?;
    let vm = st.ipam.guest_of(&mac).map(|g| g.uuid);
    let r = st
        .ipam
        .update(&name, |n| {
            if !n.contains_host(ip) || ip == n.gateway {
                return Err(format!("{ip} is not a usable address on this network"));
            }
            if let Some(other) = n.reservations.iter().find(|r| r.ip == ip && r.mac != mac) {
                return Err(format!("{ip} is already reserved for {}", other.mac));
            }
            let seen = n.reservation_for_mac(&mac).map_or(0, |r| r.seen_ms);
            n.reservations.retain(|r| r.mac != mac);
            // Two reservations in one network must not answer the same DNS
            // name, or one guest would resolve to the other's address. A
            // clash is numbered, as the allocator does.
            let host_name = host.map(|h| n.unique_name(&h, &mac));
            let r = Reservation {
                mac: mac.clone(),
                ip,
                name: host_name,
                dynamic: false,
                vm,
                seen_ms: seen,
            };
            n.reservations.push(r.clone());
            Ok(r)
        })
        .map_err(ApiError::bad_request)?;
    restart_dns(&st, &name).await?;
    Ok(Json(
        json!({"network": name, "host": reservation_json(&r, &st.ipam)}),
    ))
}

/// Drop the reservation for one MAC. The guest is not left without an address:
/// the supervisor gives it a fresh one from the range at its next pass, which
/// becomes its permanent reservation in turn.
async fn delete_host(
    State(st): State<NetState>,
    Path((name, mac)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let mac =
        store::normalize_mac(&mac).ok_or_else(|| ApiError::bad_request("not a MAC address"))?;
    managed(&st.ipam, &name)?;
    let removed = st
        .ipam
        .update(&name, |n| {
            let before = n.reservations.len();
            n.reservations.retain(|r| r.mac != mac);
            Ok(n.reservations.len() != before)
        })
        .map_err(ApiError::bad_request)?;
    restart_dns(&st, &name).await?;
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
    State(st): State<NetState>,
    axum::extract::Query(q): axum::extract::Query<FlowsQuery>,
) -> Json<Value> {
    let (flows, last, dropped) = st
        .ipam
        .flows
        .since(q.after, q.limit.unwrap_or(5000).min(20_000));
    Json(json!({"flows": flows, "last": last, "dropped": dropped}))
}

/// The network's IPAM settings, and whether this host is serving it now.
async fn get_ipam(
    State(st): State<NetState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
    let n = managed(&st.ipam, &name)?;
    let (a, b) = (u32::from(n.range_start), u32::from(n.range_end));
    let used = n
        .reservations
        .iter()
        .filter(|r| (a..=b).contains(&u32::from(r.ip)))
        .count() as u32;
    Ok(Json(json!({
        "network": name,
        "serving": st.ipam.serving(&name),
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
    State(st): State<NetState>,
    Path(name): Path<String>,
    Json(req): Json<IpamReq>,
) -> Result<Json<Value>, ApiError> {
    let name = key(&st.ipam, &name);
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
        Some(d) => Some(store::valid_domain(d).ok_or_else(|| {
            ApiError::bad_request("the domain may only contain letters, digits, '-' and '.'")
        })?),
        None => None,
    };
    managed(&st.ipam, &name)?;
    st.ipam
        .update(&name, |n| {
            n.validate_range(start, end)?;
            n.range_start = start;
            n.range_end = end;
            n.dns_forwarders = fwd;
            n.domain = domain;
            Ok(())
        })
        .map_err(ApiError::bad_request)?;
    restart_dns(&st, &name).await?;
    get_ipam(State(st), Path(name)).await
}

fn open_name(xml: &str) -> Option<String> {
    let i = xml.find("<name>")? + 6;
    let j = xml[i..].find("</name>")?;
    Some(xml[i..i + j].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> Net {
        Net {
            title: "Lab".into(),
            mode: Mode::Nat,
            bridge: String::new(),
            autostart: true,
            gateway: "10.9.0.1".parse().unwrap(),
            prefix: 24,
            range_start: "10.9.0.2".parse().unwrap(),
            range_end: "10.9.0.254".parse().unwrap(),
            dns_forwarders: Vec::new(),
            domain: None,
            lease_secs: 3600,
            reservations: Vec::new(),
        }
    }

    #[test]
    fn a_summary_reports_the_store_and_the_host_state() {
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let v = net_json(id, &net());
        assert_eq!(v["name"], id);
        assert_eq!(v["uuid"], id);
        assert_eq!(v["title"], "Lab");
        assert_eq!(v["bridge"], hostnet::bridge_name(id));
        assert_eq!(v["forward"], "nat");
        assert_eq!(v["ip"], "10.9.0.1/24");
        assert_eq!(v["persistent"], true);
        assert_eq!(v["autostart"], true);

        let mut bridge = net();
        bridge.mode = Mode::Bridge;
        bridge.bridge = "vmtest-nobr".into();
        bridge.gateway = "0.0.0.0".parse().unwrap();
        assert_eq!(net_json(id, &bridge)["forward"], "bridge");
        assert_eq!(net_json(id, &bridge)["bridge"], "vmtest-nobr");
        assert_eq!(net_json(id, &bridge)["ip"], "");
        assert_eq!(net_json(id, &bridge)["active"], false);
    }

    #[test]
    fn a_cidr_token_reads_as_a_network_and_prefix() {
        assert_eq!(
            read_cidr("10.0.0.5/24"),
            Some((u32::from(std::net::Ipv4Addr::new(10, 0, 0, 0)), 24))
        );
        assert_eq!(
            read_cidr("192.168.1.7"),
            Some((u32::from(std::net::Ipv4Addr::new(192, 168, 1, 7)), 32))
        );
        assert_eq!(read_cidr("default"), None);
        assert_eq!(read_cidr("linkdown"), None);
    }

    #[test]
    fn a_definition_name_is_read_from_the_editor_xml() {
        assert_eq!(
            open_name("<network><name>abc</name></network>").as_deref(),
            Some("abc")
        );
        assert_eq!(open_name("<network/>"), None);
    }

    fn stored(id: &str, net: Net) -> store::Store {
        let mut s = store::Store::default();
        s.networks.insert(id.to_string(), net);
        s
    }

    #[test]
    fn a_second_network_cannot_derive_the_same_bridge() {
        // Two caller-chosen ids sharing their first 12 hex: `bridge_name` can
        // still collide, so the API must refuse the second one.
        let a = "00000000-0000-4000-8000-000000000001";
        let b = "00000000-0000-4000-8000-000000000002";
        assert_eq!(
            hostnet::bridge_name(a),
            hostnet::bridge_name(b),
            "this is the case the check exists for"
        );
        let s = stored(a, net());
        assert!(check_bridge(&s, b, &net()).is_err());
        // The network itself may keep its own bridge.
        assert!(check_bridge(&s, a, &net()).is_ok());
        // Two operator bridges may share a physical LAN.
        let mut op_a = net();
        op_a.mode = Mode::Bridge;
        op_a.bridge = "br-lan".into();
        let s = stored(a, op_a.clone());
        assert!(check_bridge(&s, b, &op_a).is_ok());
        // An operator bridge may not take a managed network's derived name.
        let s = stored(a, net());
        let mut thief = net();
        thief.mode = Mode::Bridge;
        thief.bridge = hostnet::bridge_name(a);
        assert!(check_bridge(&s, b, &thief).is_err());
    }

    #[test]
    fn a_title_must_name_exactly_one_network() {
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let other = "6c03b2b6-5000-4b2d-a03f-2e4d6c8ba00f";
        let s = stored(id, net()); // title "Lab"
        assert!(check_title(&s, other, "Lab").is_err(), "duplicate title");
        assert!(
            check_title(&s, other, id).is_err(),
            "a title equal to another network's id"
        );
        assert!(check_title(&s, id, "Lab").is_ok(), "its own title");
        assert!(check_title(&s, other, "Other").is_ok());
        assert!(check_title(&s, other, "").is_ok(), "no title is fine");
    }

    #[test]
    fn a_subnet_that_overlaps_the_host_or_another_network_is_refused() {
        use std::net::Ipv4Addr;
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        // `net()` is 10.9.0.0/24; the host is on 10/8 for the first case.
        let host = [(u32::from(Ipv4Addr::new(10, 0, 0, 0)), 8)];
        let s = stored(id, net());
        assert!(
            subnet_clash(&s, &host, &net(), None).is_err(),
            "inside the host's own /8"
        );

        // Another stored network's subnet, and the edited one's own.
        let elsewhere = [(u32::from(Ipv4Addr::new(192, 168, 50, 0)), 24)];
        assert!(subnet_clash(&s, &elsewhere, &net(), None).is_err());
        assert!(subnet_clash(&s, &elsewhere, &net(), Some(id)).is_ok());

        // A fresh /24 is accepted.
        let mut fresh = net();
        fresh.gateway = "10.44.0.1".parse().unwrap();
        assert!(subnet_clash(&s, &elsewhere, &fresh, None).is_ok());
    }

    /// A shut-off guest still has a NIC naming the bridge, and it could not
    /// start after the bridge was gone, so *any* defined guest blocks the
    /// delete before anything on the host is touched (needs no root).
    #[tokio::test]
    async fn deleting_a_network_a_defined_guest_uses_is_refused() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = std::env::temp_dir().join(format!("netdel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let net = net();
        {
            let mut s = ipam.lock();
            s.networks.insert(id.into(), net.clone());
            ipam.save(&s).unwrap();
        }
        let app = routes(ipam.clone(), FirewallState::new(ipam.clone()));
        // A defined guest may not lose its bridge, running or not: the delete
        // is refused for any NIC still on it.
        for (mac, name) in [("52:54:00:aa:bb:cc", "web"), ("52:54:00:aa:bb:cd", "db")] {
            ipam.guests.lock().unwrap().clear();
            ipam.guests.lock().unwrap().insert(
                mac.into(),
                crate::ipam::server::GuestNic {
                    uuid: format!("u-{name}"),
                    name: name.into(),
                    bridge: hostnet::bridge_for(&net, id),
                },
            );
            let r = app
                .clone()
                .oneshot(
                    Request::delete(format!("/networks/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), 409, "guest {name} holds {mac}");
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&body).contains("defined guest(s)"),
                "the refusal must name what blocks it: {}",
                String::from_utf8_lossy(&body)
            );
            assert!(
                ipam.lock().networks.contains_key(id),
                "the definition is still there"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
