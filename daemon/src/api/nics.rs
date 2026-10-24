//! Guest network interfaces: hot-add, hot-remove and live rate limits.
//!
//! Every change is applied to the definition the QEMU manager stores and, when
//! the guest is running, to the live process as well, so it takes effect now
//! and survives a restart. The manager owns the QMP side; this module owns the
//! XML the form sends, and `hostnet::apply_tc` the host shaping, because only
//! the daemon may run host commands (a start installs the same plan).
//!
//! Rates are taken in Mbit/s (what people think in) and stored as KiB/s.
//! `inbound` is traffic *to* the guest (its RX), `outbound` from it (TX).

use axum::extract::{Path, State};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use super::firewall::FirewallState;
use crate::ipam::server::Ipam;
use crate::qemu::Manager;

#[derive(Clone)]
struct NicState {
    manager: Manager,
    /// The network store: a NIC names a network, and only the store knows the
    /// bridge that network runs now.
    ipam: Ipam,
    /// A NIC change adds or removes a MAC, and per-VM rules are scoped to MACs.
    fw: FirewallState,
}

pub fn routes(manager: Manager, ipam: Ipam, fw: FirewallState) -> Router {
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
        .with_state(NicState { manager, ipam, fw })
}

/// One Mbit/s in the stored `<bandwidth>` unit (KiB/s): 10^6 / 8 / 1024.
const KIB_PER_MBIT: f64 = 1_000_000.0 / 8.0 / 1024.0;
/// `average` is a 32-bit KiB/s; ~34 Tbit/s is no real limit.
const MAX_MBPS: f64 = u32::MAX as f64 / KIB_PER_MBIT;

pub fn mbps_to_kib(mbps: f64) -> u64 {
    (mbps * KIB_PER_MBIT).round() as u64
}

pub fn kib_to_mbps(kib: u64) -> f64 {
    // Rounded to 0.01 so a value set as 100 reads back as 100, not 99.99744.
    (kib as f64 / KIB_PER_MBIT * 100.0).round() / 100.0
}

/// Validate a rate. `None` or 0 means "no limit".
pub fn check_rate(v: Option<f64>, which: &str) -> Result<Option<f64>, String> {
    match v {
        None => Ok(None),
        Some(x) if !x.is_finite() || x < 0.0 => Err(format!("{which} must be a positive number")),
        Some(0.0) => Ok(None),
        Some(x) if x > MAX_MBPS => Err(format!("{which} is larger than the maximum rate")),
        // Below 1 KiB/s rounds to zero, which is read back as "unlimited".
        Some(x) if mbps_to_kib(x) == 0 => Err(format!("{which} is too small (under 1 KiB/s)")),
        Some(x) => Ok(Some(x)),
    }
}

/// A MAC in canonical lower-case colon form, unicast and not all-zero.
pub fn normalize_mac(mac: &str) -> Result<String, String> {
    crate::domainxml::normalize_mac(mac)
}

/// The `<interface>` element the add form asks for. The bridge comes from the
/// store (a derived `vbr…` name or a checked operator bridge), never from the
/// request, so nothing unvalidated is interpolated.
fn interface_xml(bridge: &str, model: &str, mac: Option<&str>) -> String {
    let mac = mac
        .map(|m| format!("<mac address='{m}'/>"))
        .unwrap_or_default();
    format!(
        "<interface type='bridge'>{mac}<source bridge='{bridge}'/><model type='{model}'/></interface>"
    )
}

/// A NIC change alters the guest's MAC set, which scopes its firewall rules.
/// Re-apply so a new NIC is covered and a removed one stops being matched.
pub(crate) async fn reapply_firewall(fw: &FirewallState) -> Option<String> {
    let _g = fw.lock.lock().await;
    let store = match crate::firewall::load(&fw.path) {
        Ok(s) => s,
        Err(e) => return Some(e),
    };
    super::firewall::apply_store(&fw.ipam, &store).await.err()
}

/// Refresh the snapshot so a new MAC has its reservation before the rules that
/// are keyed by it are rebuilt. A failed pass is logged; the supervisor's next
/// pass settles it.
async fn refresh_ipam(ipam: &Ipam, manager: &Manager) {
    if let Err(e) = crate::ipam::server::supervise_once(ipam, manager).await {
        tracing::warn!("nics: refreshing IPAM: {e}");
    }
}

fn map_err(e: String) -> ApiError {
    if e.contains("unknown QEMU") {
        ApiError::not_found(e)
    } else {
        ApiError::bad_request(e)
    }
}

#[derive(Deserialize)]
struct AddReq {
    /// A network key (its UUID or title). The NIC is attached to that
    /// network's bridge.
    network: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mac: Option<String>,
}

async fn add(
    State(s): State<NicState>,
    Path(name): Path<String>,
    Json(req): Json<AddReq>,
) -> Result<Json<Value>, ApiError> {
    let network = req.network.trim().to_string();
    if network.is_empty() || network.contains(['\'', '"', '<', '>', '&']) {
        return Err(ApiError::bad_request("choose a network"));
    }
    // Models the direct-QEMU definition can express. Anything else is refused
    // here rather than stored as a NIC QEMU cannot create.
    let model = req.model.unwrap_or_else(|| "virtio".into());
    if !matches!(model.as_str(), "virtio" | "e1000e" | "rtl8139") {
        return Err(ApiError::bad_request(format!(
            "unknown NIC model '{model}'"
        )));
    }
    let mac = match req.mac.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => Some(normalize_mac(m).map_err(ApiError::bad_request)?),
        None => None,
    };
    let bridge = s
        .ipam
        .bridge_for(&network)
        .ok_or_else(|| ApiError::bad_request(format!("unknown network '{network}'")))?;
    let xml = interface_xml(&bridge, &model, mac.as_deref());
    let (mac, live) = s.manager.add_nic_xml(&name, &xml).map_err(map_err)?;
    refresh_ipam(&s.ipam, &s.manager).await;
    let fw_err = reapply_firewall(&s.fw).await;
    Ok(Json(with_warning(
        json!({"name": name, "mac": mac, "live": live}),
        fw_err,
    )))
}

async fn remove(
    State(s): State<NicState>,
    Path((name, mac)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let pending = s.manager.remove_nic(&name, &mac).map_err(map_err)?;
    if !pending {
        // The MAC is gone from the definition, so the guest snapshot can drop
        // it in the same request rather than one supervisor pass later.
        refresh_ipam(&s.ipam, &s.manager).await;
    }
    let fw_err = reapply_firewall(&s.fw).await;
    Ok(Json(with_warning(
        json!({"name": name, "mac": mac, "removed": true, "pending": pending}),
        fw_err,
    )))
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
    State(s): State<NicState>,
    Path((name, mac)): Path<(String, String)>,
    Json(req): Json<BandwidthReq>,
) -> Result<Json<Value>, ApiError> {
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let rx = check_rate(req.rx_mbps, "RX limit").map_err(ApiError::bad_request)?;
    let tx = check_rate(req.tx_mbps, "TX limit").map_err(ApiError::bad_request)?;
    let rx_kib = rx.map(mbps_to_kib);
    let tx_kib = tx.map(mbps_to_kib);
    let tap = s
        .manager
        .set_nic_bandwidth(&name, &mac, rx_kib, tx_kib)
        .map_err(map_err)?;
    // A running bridge NIC has a tap; a stopped (or user-mode) one took the
    // definition only, and the answer says it is pending.
    if let Some(tap) = &tap {
        crate::hostnet::apply_tc(tap, rx_kib, tx_kib).map_err(ApiError::internal)?;
    }
    let live = tap.is_some();
    Ok(Json(json!({
        "name": name, "mac": mac, "live": live, "pending": !live,
        "rx_mbps": rx.map(|x| kib_to_mbps(mbps_to_kib(x))),
        "tx_mbps": tx.map(|x| kib_to_mbps(mbps_to_kib(x))),
    })))
}

/// The NIC change itself succeeded; a firewall re-apply failure is reported
/// alongside rather than turning it into an error the caller would retry.
pub(crate) fn with_warning(mut v: Value, fw_err: Option<String>) -> Value {
    if let Some(e) = fw_err {
        v["warning"] = json!(format!("firewall not re-applied: {e}"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbps_round_trips_through_kib_per_second() {
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
        assert!(
            check_rate(Some(1e12), "x").is_err(),
            "beyond the u32 KiB/s ceiling"
        );
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

    /// The add form's element names the bridge, model and MAC the manager will
    /// parse; a MAC the caller did not choose is absent, so the manager picks.
    #[test]
    fn the_add_request_builds_an_interface_element() {
        assert_eq!(
            interface_xml("vbr0", "e1000e", Some("52:54:00:aa:bb:01")),
            "<interface type='bridge'><mac address='52:54:00:aa:bb:01'/>\
             <source bridge='vbr0'/><model type='e1000e'/></interface>"
        );
        assert!(
            !interface_xml("vbr0", "virtio", None).contains("<mac"),
            "the manager generates the MAC"
        );
    }
}
