//! Guest network interfaces: hot-add, hot-remove and live rate limits.
//!
//! Every change is applied to the persistent definition and, when the guest is
//! running, to the live guest as well (`AFFECT_LIVE | AFFECT_CONFIG`), so it
//! takes effect now and survives a restart. Measured against libvirt 11 /
//! QEMU 11, three facts shape this module:
//!
//! - A rate limit set live is enforced at once: libvirt installs an htb qdisc
//!   and an ingress policer on the host tap (`tc` shows the rate), no restart.
//! - Hot-add needs a hotplug-capable bus. q35 guests (what Create VM defines)
//!   take it; an i440fx guest refuses with "Bus 'pci.0' does not support
//!   hotplugging". That error is passed through rather than hidden.
//! - Hot-remove is a *request* to the guest OS. libvirt returns success once it
//!   has asked; a guest that ignores the ACPI eject (no OS, or one that blocks
//!   it) keeps the NIC. So a removal reports `pending` when the device is still
//!   in the live XML, instead of claiming it is gone.
//!
//! Rates are taken in Mbit/s (what people think in) and stored as libvirt's
//! KiB/s. `inbound` is traffic *to* the guest (its RX), `outbound` from it (TX).

use axum::extract::{Path, State};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use virt::domain::Domain;
use virt::sys;

use super::ApiError;
use super::firewall::FirewallState;
use crate::libvirt::with_conn;

pub fn routes(fw: FirewallState) -> Router {
    Router::new()
        .route("/domains/{name}/interfaces", post(add))
        .route(
            "/domains/{name}/interfaces/{mac}",
            axum::routing::delete(remove),
        )
        .route(
            "/domains/{name}/interfaces/{mac}/bandwidth",
            put(set_bandwidth),
        )
        .with_state(fw)
}

/// One Mbit/s in libvirt's bandwidth unit (KiB/s): 10^6 / 8 / 1024.
const KIB_PER_MBIT: f64 = 1_000_000.0 / 8.0 / 1024.0;
/// libvirt caps `average` at a 32-bit KiB/s; ~34 Tbit/s is no real limit.
const MAX_MBPS: f64 = u32::MAX as f64 / KIB_PER_MBIT;

pub fn mbps_to_kib(mbps: f64) -> u64 {
    (mbps * KIB_PER_MBIT).round() as u64
}

pub fn kib_to_mbps(kib: u64) -> f64 {
    // Rounded to 0.01 so a value set as 100 reads back as 100, not 99.99744.
    (kib as f64 / KIB_PER_MBIT * 100.0).round() / 100.0
}

/// Validate a rate. `None` or 0 means "no limit".
fn check_rate(v: Option<f64>, which: &str) -> Result<Option<f64>, String> {
    match v {
        None => Ok(None),
        Some(x) if !x.is_finite() || x < 0.0 => Err(format!("{which} must be a positive number")),
        Some(0.0) => Ok(None),
        Some(x) if x > MAX_MBPS => Err(format!("{which} is larger than libvirt accepts")),
        // Below 1 KiB/s rounds to zero, which libvirt reads as "unlimited".
        Some(x) if mbps_to_kib(x) == 0 => Err(format!("{which} is too small (under 1 KiB/s)")),
        Some(x) => Ok(Some(x)),
    }
}

/// A MAC in canonical lower-case colon form, or an error.
pub fn normalize_mac(mac: &str) -> Result<String, String> {
    let m = mac.trim().to_ascii_lowercase();
    let parts: Vec<&str> = m.split(':').collect();
    let ok = parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !ok {
        return Err(format!("'{mac}' is not a MAC address"));
    }
    // The low bit of the first octet marks multicast, which a NIC cannot use.
    if u8::from_str_radix(parts[0], 16)
        .map(|b| b & 1 == 1)
        .unwrap_or(true)
    {
        return Err(format!("'{mac}' is a multicast address"));
    }
    Ok(m)
}

/// The `<interface>...</interface>` block with this MAC, verbatim.
pub fn interface_block<'a>(xml: &'a str, mac: &str) -> Option<&'a str> {
    let mut rest = xml;
    let mut offset = 0;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let end = block.find("</interface>")? + "</interface>".len();
        let b = &block[..end];
        if b.to_ascii_lowercase().contains(&format!("'{mac}'"))
            || b.to_ascii_lowercase().contains(&format!("\"{mac}\""))
        {
            return Some(&xml[offset + i..offset + i + end]);
        }
        offset += i + end;
        rest = &xml[offset..];
    }
    None
}

/// Replace (or add, or drop) the `<bandwidth>` of an interface block.
pub fn with_bandwidth(block: &str, inbound_kib: Option<u64>, outbound_kib: Option<u64>) -> String {
    // Remove any existing <bandwidth>, in either shape.
    let mut out = block.to_string();
    if let Some(s) = out.find("<bandwidth") {
        let tail = &out[s..];
        let end = if let Some(e) = tail.find("</bandwidth>") {
            e + "</bandwidth>".len()
        } else {
            tail.find("/>").map(|e| e + 2).unwrap_or(tail.len())
        };
        // Also swallow the indentation/newline in front of it.
        let start = out[..s].trim_end_matches([' ', '\t']).len();
        let start = if out[..start].ends_with('\n') {
            start - 1
        } else {
            start
        };
        out.replace_range(start..s + end, "");
    }
    if inbound_kib.is_none() && outbound_kib.is_none() {
        return out;
    }
    let mut bw = String::from("\n  <bandwidth>");
    if let Some(k) = inbound_kib {
        bw.push_str(&format!("\n    <inbound average='{k}'/>"));
    }
    if let Some(k) = outbound_kib {
        bw.push_str(&format!("\n    <outbound average='{k}'/>"));
    }
    bw.push_str("\n  </bandwidth>\n");
    let close = out.rfind("</interface>").unwrap_or(out.len());
    out.insert_str(close, &bw);
    out
}

/// Read an `average` from a direction tag inside `<bandwidth>`.
pub fn bandwidth_of(block: &str) -> (Option<u64>, Option<u64>) {
    let Some(s) = block.find("<bandwidth") else {
        return (None, None);
    };
    let bw = &block[s..];
    let read = |dir: &str| -> Option<u64> {
        let t = &bw[bw.find(&format!("<{dir}"))?..];
        let t = &t[..t.find('>')?];
        let a = &t[t.find("average=")? + "average=".len()..];
        let q = a.chars().next()?;
        let a = &a[1..];
        a[..a.find(q)?].parse().ok()
    };
    (read("inbound"), read("outbound"))
}

/// Flags for a device change: always the persistent definition, and the live
/// guest too when it is running.
fn affect(d: &Domain) -> u32 {
    let live = d.is_active().unwrap_or(false);
    sys::VIR_DOMAIN_AFFECT_CONFIG | if live { sys::VIR_DOMAIN_AFFECT_LIVE } else { 0 }
}

/// A NIC change alters the guest's MAC set, which scopes its firewall rules.
/// Re-apply so a new NIC is covered and a removed one stops being matched.
async fn reapply_firewall(fw: &FirewallState) -> Option<String> {
    let _g = fw.lock.lock().await;
    let store = match crate::firewall::load(&fw.path) {
        Ok(s) => s,
        Err(e) => return Some(e),
    };
    super::firewall::apply_store(store).await.err()
}

#[derive(Deserialize)]
struct AddReq {
    network: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mac: Option<String>,
}

async fn add(
    State(fw): State<FirewallState>,
    Path(name): Path<String>,
    Json(req): Json<AddReq>,
) -> Result<Json<Value>, ApiError> {
    let network = req.network.trim().to_string();
    if network.is_empty() || network.contains(['\'', '"', '<', '>', '&']) {
        return Err(ApiError::bad_request("choose a network"));
    }
    let model = req.model.unwrap_or_else(|| "virtio".into());
    if !matches!(model.as_str(), "virtio" | "e1000e" | "e1000" | "rtl8139") {
        return Err(ApiError::bad_request(format!(
            "unknown NIC model '{model}'"
        )));
    }
    let mac = match req.mac.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => Some(normalize_mac(m).map_err(ApiError::bad_request)?),
        None => None,
    };
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let live = d.is_active().unwrap_or(false);
        let mac_xml = mac
            .as_deref()
            .map(|m| format!("<mac address='{m}'/>"))
            .unwrap_or_default();
        let xml = format!(
            "<interface type='network'>{mac_xml}<source network='{network}'/><model type='{model}'/></interface>"
        );
        d.attach_device_flags(&xml, affect(&d))
            .map_err(|e| hotplug_hint(&e.to_string(), live))?;
        // libvirt picks the MAC when none was given; report the one it chose
        // (the last interface on the network is the one just added).
        let after = d.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)?;
        let chosen = mac.clone().or_else(|| {
            super::firewall::domain_macs(&after).into_iter().last()
        });
        Ok(json!({"name": name, "mac": chosen, "live": live}))
    })
    .await
    .map_err(ApiError::bad_request)?;
    let fw_err = reapply_firewall(&fw).await;
    Ok(Json(with_warning(v, fw_err)))
}

async fn remove(
    State(fw): State<FirewallState>,
    Path((name, mac)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let live = d.is_active().unwrap_or(false);
        // Detach by the definition's own block, so libvirt matches it exactly.
        let config = d.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)?;
        let live_xml = d.get_xml_desc(0)?;
        let block = interface_block(&config, &mac)
            .or_else(|| interface_block(&live_xml, &mac))
            .ok_or_else(|| format!("no interface with MAC {mac}"))?
            .to_string();
        let mut flags = sys::VIR_DOMAIN_AFFECT_CONFIG;
        if live && interface_block(&live_xml, &mac).is_some() {
            flags |= sys::VIR_DOMAIN_AFFECT_LIVE;
        }
        if interface_block(&config, &mac).is_none() {
            // Only in the live guest (hot-added without config): live only.
            flags = sys::VIR_DOMAIN_AFFECT_LIVE;
        }
        d.detach_device_flags(&block, flags)?;
        // The guest may still be holding it: an unplug is only a request.
        let pending = live && interface_block(&d.get_xml_desc(0)?, &mac).is_some();
        Ok(json!({"name": name, "mac": mac, "removed": true, "pending": pending}))
    })
    .await
    .map_err(ApiError::bad_request)?;
    let fw_err = reapply_firewall(&fw).await;
    Ok(Json(with_warning(v, fw_err)))
}

#[derive(Deserialize)]
struct BandwidthReq {
    /// Traffic to the guest (its RX), Mbit/s. Absent or 0 = unlimited.
    #[serde(default)]
    rx_mbps: Option<f64>,
    /// Traffic from the guest (its TX), Mbit/s. Absent or 0 = unlimited.
    #[serde(default)]
    tx_mbps: Option<f64>,
}

async fn set_bandwidth(
    Path((name, mac)): Path<(String, String)>,
    Json(req): Json<BandwidthReq>,
) -> Result<Json<Value>, ApiError> {
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let rx = check_rate(req.rx_mbps, "RX limit").map_err(ApiError::bad_request)?;
    let tx = check_rate(req.tx_mbps, "TX limit").map_err(ApiError::bad_request)?;
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let live = d.is_active().unwrap_or(false);
        // Edit the persistent block: it is the one that survives a restart,
        // and update-device matches the live NIC by its MAC.
        let config = d.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)?;
        let block =
            interface_block(&config, &mac).ok_or_else(|| format!("no interface with MAC {mac}"))?;
        let updated = with_bandwidth(block, rx.map(mbps_to_kib), tx.map(mbps_to_kib));
        d.update_device_flags(&updated, affect(&d))?;
        Ok(json!({
            "name": name, "mac": mac, "live": live,
            "rx_mbps": rx.map(|x| kib_to_mbps(mbps_to_kib(x))),
            "tx_mbps": tx.map(|x| kib_to_mbps(mbps_to_kib(x))),
        }))
    })
    .await
    .map_err(ApiError::bad_request)?;
    Ok(Json(v))
}

/// Turn libvirt's hot-plug failures into something actionable. Both mean the
/// running guest has nowhere to put the device; the persistent definition is
/// untouched, so the fix is to add it while the guest is shut off (libvirt
/// then creates the slot), or to give the guest spare PCIe root ports.
fn hotplug_hint(err: &str, live: bool) -> String {
    let no_slot = err.contains("No more available PCI slots");
    let no_hotplug = err.contains("does not support hotplugging");
    if live && (no_slot || no_hotplug) {
        let why = if no_slot {
            "the running guest has no free PCIe slot"
        } else {
            "this guest's machine type cannot hot-plug devices"
        };
        return format!(
            "{why}; shut it down and add the interface then (guests created by \
             virt-man have spare slots for hot-plug). libvirt: {err}"
        );
    }
    err.to_string()
}

/// The NIC change itself succeeded; a firewall re-apply failure is reported
/// alongside rather than turning it into an error the caller would retry.
fn with_warning(mut v: Value, fw_err: Option<String>) -> Value {
    if let Some(e) = fw_err {
        v["warning"] = json!(format!("firewall not re-applied: {e}"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = "<domain><devices>
    <interface type='network'>
      <mac address='52:54:00:aa:bb:01'/>
      <source network='internet'/>
      <model type='virtio'/>
    </interface>
    <interface type='network'>
      <mac address='52:54:00:aa:bb:02'/>
      <source network='internal'/>
      <model type='virtio'/>
      <bandwidth>
        <inbound average='1250'/>
        <outbound average='2500'/>
      </bandwidth>
    </interface>
  </devices></domain>";

    #[test]
    fn hotplug_failures_explain_the_fix() {
        let e = "internal error: No more available PCI slots";
        assert!(hotplug_hint(e, true).starts_with("the running guest has no free PCIe slot"));
        let e =
            "unable to execute QEMU command 'device_add': Bus 'pci.0' does not support hotplugging";
        assert!(hotplug_hint(e, true).contains("cannot hot-plug"));
        assert_eq!(
            hotplug_hint(e, false),
            e,
            "a shut-off guest's error is passed through"
        );
        assert_eq!(hotplug_hint("other", true), "other");
    }

    #[test]
    fn mbps_round_trips_through_libvirts_unit() {
        assert_eq!(mbps_to_kib(10.0), 1221, "10 Mbit/s = 1220.7 KiB/s");
        for m in [1.0, 10.0, 100.0, 250.0, 1000.0] {
            assert_eq!(
                kib_to_mbps(mbps_to_kib(m)),
                m,
                "{m} Mbit/s reads back as set"
            );
        }
    }

    #[test]
    fn rates_are_validated() {
        assert_eq!(check_rate(None, "x"), Ok(None));
        assert_eq!(check_rate(Some(0.0), "x"), Ok(None), "0 means unlimited");
        assert_eq!(check_rate(Some(50.0), "x"), Ok(Some(50.0)));
        assert!(check_rate(Some(-1.0), "x").is_err());
        assert!(check_rate(Some(f64::NAN), "x").is_err());
        assert!(check_rate(Some(1e12), "x").is_err(), "beyond libvirt's u32");
        assert!(
            check_rate(Some(0.001), "x").is_err(),
            "rounds to 0 KiB/s = unlimited"
        );
    }

    #[test]
    fn macs_are_normalised_and_validated() {
        assert_eq!(
            normalize_mac(" 52:54:00:AA:bb:01 ").unwrap(),
            "52:54:00:aa:bb:01"
        );
        assert!(normalize_mac("52:54:00:aa:bb").is_err());
        assert!(normalize_mac("52:54:00:aa:bb:zz").is_err());
        assert!(normalize_mac("01:00:5e:00:00:01").is_err(), "multicast");
        assert!(
            normalize_mac("52:54:00:aa:bb:01'/><x").is_err(),
            "no XML injection"
        );
    }

    #[test]
    fn the_right_interface_block_is_found_by_mac() {
        let b = interface_block(XML, "52:54:00:aa:bb:02").unwrap();
        assert!(b.contains("internal") && !b.contains("internet"));
        assert!(b.starts_with("<interface") && b.ends_with("</interface>"));
        assert!(
            interface_block(XML, "52:54:00:aa:bb:01")
                .unwrap()
                .contains("internet")
        );
        assert!(interface_block(XML, "52:54:00:aa:bb:99").is_none());
    }

    #[test]
    fn bandwidth_is_read_per_direction() {
        let b = interface_block(XML, "52:54:00:aa:bb:02").unwrap();
        assert_eq!(bandwidth_of(b), (Some(1250), Some(2500)));
        let none = interface_block(XML, "52:54:00:aa:bb:01").unwrap();
        assert_eq!(bandwidth_of(none), (None, None));
    }

    #[test]
    fn setting_bandwidth_replaces_rather_than_duplicates() {
        let b = interface_block(XML, "52:54:00:aa:bb:02").unwrap();
        let out = with_bandwidth(b, Some(100), None);
        assert_eq!(out.matches("<bandwidth>").count(), 1, "old block removed");
        assert_eq!(bandwidth_of(&out), (Some(100), None), "only inbound left");
        assert!(
            out.contains("<source network='internal'/>"),
            "rest untouched"
        );
        assert!(out.trim_end().ends_with("</interface>"));
    }

    #[test]
    fn clearing_both_limits_drops_the_bandwidth_element() {
        let b = interface_block(XML, "52:54:00:aa:bb:02").unwrap();
        let out = with_bandwidth(b, None, None);
        assert!(!out.contains("bandwidth"), "{out}");
        assert!(out.contains("<mac address='52:54:00:aa:bb:02'/>"));
    }

    #[test]
    fn adding_bandwidth_to_an_unlimited_nic() {
        let b = interface_block(XML, "52:54:00:aa:bb:01").unwrap();
        let out = with_bandwidth(b, None, Some(42));
        assert_eq!(bandwidth_of(&out), (None, Some(42)));
    }
}
