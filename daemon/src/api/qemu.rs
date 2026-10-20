use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::ApiError;
use super::firewall::FirewallState;
use crate::domainxml::{self, IfaceKind, Interface};
use crate::ipam::server::Ipam;
use crate::qemu::{DiskFormat, Manager, State as VmState, VmConfig};

#[derive(Clone)]
struct QemuState {
    manager: Manager,
    /// The network store: a bridge NIC names a stored network, and only the
    /// store knows which bridge that network runs now.
    ipam: Ipam,
    /// A NIC change adds or removes a MAC, and per-VM rules are scoped to MACs.
    fw: FirewallState,
}

fn err(e: String) -> ApiError {
    if e == "unknown QEMU VM" {
        ApiError::not_found(e)
    } else {
        ApiError::conflict(e)
    }
}

#[derive(Deserialize)]
struct Define {
    /// The SPA's domain XML, the definition the manager stores.
    xml: String,
}

/// A NIC as a request: the shape the stored config used to answer with.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum NetworkSpec {
    User {
        #[serde(default)]
        hostfwd: Vec<crate::qemu_network::HostForward>,
    },
    Bridge {
        network: String,
        #[serde(default)]
        mac: Option<String>,
    },
}

/// The NIC attachment a resolved request asks for: no bridge is QEMU
/// user-mode; a MAC is used when the caller names one.
struct NicLink {
    bridge: Option<String>,
    mac: String,
}

/// Turn a request's spec into a bridge (or user mode). A bridge NIC names a
/// stored network by key or title; the bridge is derived from the store and
/// never taken from the client, so a guest cannot be attached to an arbitrary
/// host interface.
fn resolve(s: &QemuState, spec: NetworkSpec) -> Result<NicLink, ApiError> {
    Ok(match spec {
        NetworkSpec::User { hostfwd } => {
            // Legacy hostfwd input is rejected: inbound policy has its own
            // endpoint, and user-mode port forwarding is not part of the
            // domain definition this backend runs.
            if !hostfwd.is_empty() {
                return Err(ApiError::bad_request(
                    "hostfwd must be configured through the firewall allowlist",
                ));
            }
            NicLink {
                bridge: None,
                mac: String::new(),
            }
        }
        NetworkSpec::Bridge { network, mac } => {
            let key = s.ipam.resolve_key(&network).ok_or_else(|| {
                ApiError::not_found(format!("network '{network}' is not managed here"))
            })?;
            let bridge = s
                .ipam
                .bridge_for(&key)
                .ok_or_else(|| ApiError::not_found(format!("network '{network}' has no bridge")))?;
            NicLink {
                bridge: Some(bridge),
                mac: mac.unwrap_or_default(),
            }
        }
    })
}

fn config(s: &QemuState, id: &str) -> Result<VmConfig, ApiError> {
    s.manager
        .list()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("unknown QEMU VM"))
}

fn primary(s: &QemuState, id: &str) -> Result<Interface, ApiError> {
    domainxml::parse(&config(s, id)?.xml)
        .map_err(ApiError::internal)?
        .interfaces
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::bad_request("the VM has no NIC"))
}

/// A stored bridge can go stale: editing a network's mode, or deleting and
/// re-creating it, changes the bridge it derives. Re-resolve every bridge NIC
/// from the store before a start, so the tap lands on the network that exists
/// now — otherwise a stale name could attach the guest to the operator's LAN.
/// A stopped guest is updated in place; a running one is left alone, because
/// its tap is already on the old bridge and the record describes what is live.
fn rebind(s: &QemuState, id: &str) -> Result<(), ApiError> {
    let cfg = config(s, id)?;
    let domain = domainxml::parse(&cfg.xml).map_err(ApiError::internal)?;
    let stopped = s.manager.state(id).map_err(err)? == VmState::Stopped;
    for iface in domain.interfaces {
        let IfaceKind::Bridge(bridge) = iface.kind else {
            continue;
        };
        let Some(mac) = iface.mac else {
            continue;
        };
        let key = s.ipam.network_for_bridge(&bridge).ok_or_else(|| {
            ApiError::conflict(format!("bridge '{bridge}' belongs to no network here"))
        })?;
        let now = s
            .ipam
            .bridge_for(&key)
            .ok_or_else(|| ApiError::conflict(format!("network '{key}' is not managed here")))?;
        if stopped && now != bridge {
            s.manager
                .set_nic_bridge(id, &mac, Some(&now))
                .map_err(err)?;
        }
    }
    Ok(())
}

/// Per-VM rules are scoped to MACs, so a NIC change must reach the live
/// ruleset. The change itself has already been accepted: a failed re-apply is
/// logged, and the next apply (or a network lifecycle change) rebuilds it.
async fn reapply(s: &QemuState) {
    if let Err(e) = s.fw.reapply().await {
        tracing::warn!("qemu: re-applying the firewall after a NIC change: {e}");
    }
}

#[derive(Serialize)]
struct Row {
    id: String,
    title: String,
    state: &'static str,
    autostart: bool,
}

async fn list(State(s): State<QemuState>) -> Result<Json<Vec<Row>>, ApiError> {
    let mut rows = Vec::new();
    for cfg in s.manager.list().map_err(ApiError::internal)? {
        let state = match s.manager.state(&cfg.id).map_err(err)? {
            VmState::Running => "running",
            VmState::Paused => "paused",
            VmState::Stopped => "stopped",
            VmState::Unknown => "unknown",
        };
        rows.push(Row {
            id: cfg.id,
            title: cfg.title,
            state,
            autostart: cfg.autostart,
        });
    }
    Ok(Json(rows))
}

async fn define(
    State(s): State<QemuState>,
    Json(body): Json<Define>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = s.manager.define(&body.xml).map_err(err)?;
    Ok(Json(
        json!({"ok": true, "id": config.id, "title": config.title}),
    ))
}

async fn xml(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        json!({"id": id, "xml": s.manager.xml(&id).map_err(err)?}),
    ))
}

async fn put_xml(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<Define>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.manager.update_xml(&id, &body.xml).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}

/// User-mode port forwarding is not part of the domain XML, so this backend
/// has no allowlist to change. The route keeps its shape and says so, rather
/// than accepting rules that would never reach a NIC.
async fn firewall() -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!({
        "policy": "hostfwd_allowlist",
        "guest_filtering": "unsupported",
        "egress_filtering": "unsupported",
        "rules": [],
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetFirewall {
    #[serde(default)]
    rules: Vec<crate::qemu_network::HostForward>,
}

async fn set_firewall(
    State(_s): State<QemuState>,
    Path(_id): Path<String>,
    Json(body): Json<SetFirewall>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !body.rules.is_empty() {
        return Err(ApiError::bad_request(
            "user-mode port forwarding is not part of the domain definition; configure it on the guest",
        ));
    }
    firewall().await
}

async fn network(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let domain = domainxml::parse(&config(&s, &id)?.xml).map_err(ApiError::internal)?;
    let Some(iface) = domain.interfaces.into_iter().next() else {
        return Ok(Json(json!({"mode": "none"})));
    };
    Ok(Json(match iface.kind {
        IfaceKind::User => json!({"mode": "user", "hostfwd": [], "mac": iface.mac}),
        IfaceKind::Bridge(bridge) => {
            json!({"mode": "bridge", "bridge": bridge, "mac": iface.mac})
        }
    }))
}

async fn state(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let state = s.manager.state(&id).map_err(err)?;
    let state = match state {
        VmState::Running => "running",
        VmState::Paused => "paused",
        VmState::Stopped => "stopped",
        VmState::Unknown => "unknown",
    };
    Ok(Json(json!({"id": id, "state": state})))
}

async fn set_network(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<NetworkSpec>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let link = resolve(&s, body)?;
    let mac = primary(&s, &id)?
        .mac
        .ok_or_else(|| ApiError::bad_request("the VM's NIC has no MAC address"))?;
    s.manager
        .set_nic_bridge(&id, &mac, link.bridge.as_deref())
        .map_err(err)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct Nic {
    network: NetworkSpec,
}

async fn add_nic(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<Nic>,
) -> Result<Json<serde_json::Value>, ApiError> {
    rebind(&s, &id)?;
    let link = resolve(&s, body.network)?;
    let mac = s
        .manager
        .add_nic(&id, link.bridge.as_deref(), &link.mac)
        .map_err(err)?;
    // The new MAC is part of the guest's rules from now on. Refresh the guest
    // snapshot first, so the reservation exists before the rules that are keyed
    // by it are rebuilt instead of reading the 5 s-old map. No manager lock is
    // held here; a failed pass is logged and the next one settles it.
    if let Err(e) = crate::ipam::server::supervise_once(&s.ipam, &s.manager).await {
        tracing::warn!("qemu: refreshing IPAM after adding a NIC: {e}");
    }
    reapply(&s).await;
    Ok(Json(json!({"ok": true, "mac": mac})))
}

async fn remove_nic(
    State(s): State<QemuState>,
    Path((id, mac)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mac = domainxml::normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let pending = s.manager.remove_nic(&id, &mac).map_err(err)?;
    if !pending {
        // The MAC is gone from the guest snapshot, so the per-VM rules can drop
        // it in the same request rather than one supervisor pass later.
        if let Err(e) = crate::ipam::server::supervise_once(&s.ipam, &s.manager).await {
            tracing::warn!("qemu: refreshing IPAM after removing a NIC: {e}");
        }
        reapply(&s).await;
    }
    Ok(Json(
        json!({"ok": !pending, "pending": pending, "mac": mac}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddDisk {
    path: std::path::PathBuf,
    #[serde(default)]
    format: Option<String>,
}

async fn add_disk(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<AddDisk>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let format = match body.format.as_deref().unwrap_or("raw") {
        "raw" => DiskFormat::Raw,
        "qcow2" => DiskFormat::Qcow2,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown disk format '{other}'"
            )));
        }
    };
    let (target, path) = s.manager.add_disk(&id, &body.path, format).map_err(err)?;
    Ok(Json(json!({"ok": true, "target": target, "path": path})))
}

async fn remove_disk(
    State(s): State<QemuState>,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = s.manager.remove_disk(&id, &target).map_err(err)?;
    Ok(Json(
        json!({"ok": true, "target": target, "pending": pending}),
    ))
}
async fn start(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    rebind(&s, &id)?;
    s.manager.start(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn shutdown(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.manager.powerdown(&id, false).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn force_stop(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.manager.force_stop(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn remove(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.manager.delete(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}

pub fn routes(manager: Manager, ipam: Ipam, fw: FirewallState) -> Router {
    Router::new()
        .route("/qemu/vms", get(list).post(define))
        .route("/qemu/vms/{id}", delete(remove))
        .route("/qemu/vms/{id}/xml", get(xml).put(put_xml))
        .route("/qemu/vms/{id}/state", get(state))
        .route("/qemu/vms/{id}/network", get(network).put(set_network))
        .route("/qemu/vms/{id}/firewall", get(firewall).put(set_firewall))
        .route("/qemu/vms/{id}/nics", post(add_nic))
        .route("/qemu/vms/{id}/nics/{mac}", delete(remove_nic))
        .route("/qemu/vms/{id}/disks", post(add_disk))
        .route("/qemu/vms/{id}/disks/{target}", delete(remove_disk))
        .route("/qemu/vms/{id}/start", post(start))
        .route("/qemu/vms/{id}/shutdown", post(shutdown))
        .route("/qemu/vms/{id}/force-stop", post(force_stop))
        .with_state(QemuState { manager, ipam, fw })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qemu-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A manager whose VMs never launch: the temp dirs hold metadata only.
    fn manager(dir: &std::path::Path) -> Manager {
        Manager::with_qemu(dir, dir.join("state"), "/nonexistent/qemu").unwrap()
    }

    fn ipam(dir: &std::path::Path) -> Ipam {
        Ipam::load(dir.join("networks.json"), dir.to_path_buf()).unwrap()
    }

    fn app(dir: &std::path::Path) -> Router {
        let ip = ipam(dir);
        routes(manager(dir), ip.clone(), FirewallState::new(ip))
    }

    fn state(dir: &std::path::Path) -> QemuState {
        let ip = ipam(dir);
        QemuState {
            manager: manager(dir),
            ipam: ip.clone(),
            fw: FirewallState::new(ip),
        }
    }

    fn disk(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    const VM: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn domain_xml(bridge: &str, disk: &std::path::Path) -> String {
        format!(
            r#"<domain type="kvm">
  <name>{VM}</name>
  <uuid>{VM}</uuid>
  <title>web</title>
  <memory unit="KiB">131072</memory>
  <vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os>
  <devices>
    <disk type="file" device="disk">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <interface type="bridge">
      <source bridge="{bridge}"/>
      <model type="virtio"/>
      <mac address="52:54:00:aa:bb:01"/>
    </interface>
  </devices>
</domain>"#,
            disk.display()
        )
    }

    async fn put(app: Router, body: &str) -> (u16, serde_json::Value) {
        call(app, "PUT", &format!("/qemu/vms/{VM}/network"), body).await
    }

    async fn call(app: Router, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let r = app.oneshot(req).await.unwrap();
        let status = r.status().as_u16();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    /// A bridge NIC is resolved through the network store before the manager
    /// sees it: an unknown network is a 404, not an attachment to whatever
    /// interface the client typed.
    #[tokio::test]
    async fn a_bridge_nic_names_a_stored_network() {
        let dir = temp("bridge");
        let (status, body) = put(app(&dir), r#"{"mode":"bridge","network":"nope"}"#).await;
        assert_eq!(status, 404, "{body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("nope"),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// User mode keeps its wire shape, and a legacy hostfwd body is refused:
    /// inbound policy has its own endpoint.
    #[tokio::test]
    async fn user_mode_keeps_its_shape_and_refuses_hostfwd() {
        let dir = temp("user");
        let router = app(&dir);
        let (status, body) = put(
            router.clone(),
            r#"{"mode":"user","hostfwd":[{"protocol":"tcp","host_port":2222,"guest_ip":"192.0.2.2","guest_port":22}]}"#,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        // With no VM defined, a well-formed user request reaches the manager.
        let (status, body) = put(router, r#"{"mode":"user"}"#).await;
        assert_eq!(status, 404, "{body}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A network's mode edit or re-create changes its derived bridge. Before a
    /// start the stored bridge is re-read from the store, so the tap lands on
    /// the network that exists now rather than a stale name that may by now be
    /// the operator's LAN bridge. A network that no longer exists is refused.
    #[test]
    fn a_stale_bridge_is_resolved_again_from_the_store() {
        let dir = temp("rebind");
        let s = state(&dir);
        let net_id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let primary = disk(&dir, "disk");
        s.manager
            .define(&domain_xml("vbrstale00000", &primary))
            .unwrap();
        s.ipam.lock().networks.insert(
            net_id.into(),
            crate::ipam::store::import(
                "<network><title>lab</title><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
            )
            .unwrap(),
        );
        let derived = crate::hostnet::bridge_name(net_id);
        // A network whose current bridge differs from the stored one is
        // refused until the network's own bridge is what the guest names.
        assert!(
            rebind(&s, VM).is_err(),
            "the stored bridge belongs to no network"
        );
        s.manager
            .set_nic_bridge(VM, "52:54:00:aa:bb:01", Some(&derived))
            .unwrap();
        assert!(rebind(&s, VM).is_ok());
        assert_eq!(
            domainxml::bridges(&s.manager.xml(VM).unwrap()).unwrap()[0].bridge,
            derived
        );
        // A network that has gone away is refused, not started on the old name.
        s.ipam.lock().networks.clear();
        assert!(rebind(&s, VM).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The primary NIC answers in the shape the Network tab reads, and a VM
    /// with no NIC says so.
    #[tokio::test]
    async fn the_primary_nic_answers_as_its_wire_shape() {
        let dir = temp("shape");
        let s = state(&dir);
        let primary = disk(&dir, "disk");
        s.manager.define(&domain_xml("vbr0", &primary)).unwrap();
        let Json(body) = network(State(s), Path(VM.into()))
            .await
            .ok()
            .expect("the primary NIC answers");
        assert_eq!(
            body,
            serde_json::json!({"mode":"bridge","bridge":"vbr0","mac":"52:54:00:aa:bb:01"})
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A disk request is refused before it reaches the manager when its format
    /// is not one QEMU could be given; an unknown VM is a 404, and removal of
    /// an unknown disk likewise.
    #[tokio::test]
    async fn a_disk_request_validates_its_format() {
        let dir = temp("disk");
        let extra = disk(&dir, "extra");
        let path = format!("/qemu/vms/{VM}/disks");
        let body = |format: &str| serde_json::json!({"path": extra, "format": format}).to_string();
        let router = app(&dir);
        let (status, b) = call(router.clone(), "POST", &path, &body("qcow3")).await;
        assert_eq!(status, 400, "{b}");
        let (status, b) = call(router.clone(), "POST", &path, &body("qcow2")).await;
        assert_eq!(status, 404, "{b}: no VM is defined");
        let (status, b) = call(router, "DELETE", &format!("/qemu/vms/{VM}/disks/vdb"), "").await;
        assert_eq!(status, 404, "{b}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
