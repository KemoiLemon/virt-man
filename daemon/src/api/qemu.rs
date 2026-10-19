use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use super::firewall::FirewallState;
use crate::ipam::server::Ipam;
use crate::qemu::{DiskFormat, Manager, Network, State as VmState, VmConfig};
use crate::qemu_network::HostForward;

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
    id: String,
    #[serde(default)]
    title: String,
    disk: std::path::PathBuf,
    memory_mib: u32,
    vcpus: u16,
}

/// A NIC as a request or response: the same shape the stored config has.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum NetworkSpec {
    User {
        #[serde(default)]
        hostfwd: Vec<HostForward>,
    },
    Bridge {
        network: String,
        #[serde(default)]
        mac: Option<String>,
    },
}

/// Turn a request's spec into a stored network. A bridge NIC names a stored
/// network by key or title; the bridge is derived from the store and never
/// taken from the client, so a guest cannot be attached to an arbitrary host
/// interface.
fn resolve(s: &QemuState, spec: NetworkSpec) -> Result<Network, ApiError> {
    Ok(match spec {
        NetworkSpec::User { hostfwd } => {
            // Legacy hostfwd input is rejected: inbound policy has its own
            // admin-only endpoint.
            if !hostfwd.is_empty() {
                return Err(ApiError::bad_request(
                    "hostfwd must be configured through the firewall allowlist",
                ));
            }
            Network::User { hostfwd }
        }
        NetworkSpec::Bridge { network, mac } => {
            let key = s.ipam.resolve_key(&network).ok_or_else(|| {
                ApiError::not_found(format!("network '{network}' is not managed here"))
            })?;
            let bridge = s
                .ipam
                .bridge_for(&key)
                .ok_or_else(|| ApiError::not_found(format!("network '{network}' has no bridge")))?;
            Network::Bridge {
                network: key,
                bridge,
                mac: mac.unwrap_or_default(),
            }
        }
    })
}

/// A stored bridge can go stale: editing a network's mode, or deleting and
/// re-creating it, changes the bridge it derives. Re-resolve every bridge NIC
/// from the store before a start, so the tap lands on the network that exists
/// now — otherwise a stale name could attach the guest to the operator's LAN.
/// A stopped guest is updated in place; a running one is left alone, because
/// its tap is already on the old bridge and the record describes what is live.
fn rebind(s: &QemuState, id: &str) -> Result<(), ApiError> {
    let config = s
        .manager
        .list()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("unknown QEMU VM"))?;
    let stopped = s.manager.state(id).map_err(err)? == VmState::Stopped;
    for (nic_id, network) in config.nics() {
        let Network::Bridge {
            network, bridge, ..
        } = network
        else {
            continue;
        };
        let now = s.ipam.bridge_for(network).ok_or_else(|| {
            ApiError::conflict(format!("network '{network}' is not managed here"))
        })?;
        if stopped && &now != bridge {
            s.manager.rebind_bridge(id, nic_id, &now).map_err(err)?;
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

async fn list(State(s): State<QemuState>) -> Result<Json<Vec<VmConfig>>, ApiError> {
    s.manager.list().map(Json).map_err(ApiError::internal)
}
async fn define(
    State(s): State<QemuState>,
    Json(body): Json<Define>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.manager
        .define(VmConfig {
            id: body.id,
            title: body.title,
            disk: body.disk,
            memory_mib: body.memory_mib,
            vcpus: body.vcpus,
            network: Default::default(),
            firewall: Default::default(),
            extra_nics: vec![],
            extra_disks: vec![],
        })
        .map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn firewall(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = s
        .manager
        .list()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("unknown QEMU VM"))?;
    Ok(Json(
        json!({"policy":"hostfwd_allowlist", "guest_filtering":"unsupported", "egress_filtering":"unsupported", "rules":config.firewall.hostfwd}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetFirewall {
    #[serde(default)]
    rules: Vec<crate::qemu_network::HostForward>,
}
async fn set_firewall(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<SetFirewall>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let policy = crate::qemu_network::UserNetwork {
        hostfwd: body.rules,
    };
    s.manager.set_firewall(&id, policy.clone()).map_err(err)?;
    Ok(Json(
        json!({"policy":"hostfwd_allowlist", "guest_filtering":"unsupported", "egress_filtering":"unsupported", "rules":policy.hostfwd}),
    ))
}
async fn network(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = s
        .manager
        .list()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("unknown QEMU VM"))?;
    Ok(Json(json!(config.network)))
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
    let network = resolve(&s, body)?;
    s.manager.set_network(&id, network).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
#[derive(Deserialize)]
struct Nic {
    device_id: String,
    network: NetworkSpec,
}
async fn add_nic(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<Nic>,
) -> Result<Json<serde_json::Value>, ApiError> {
    rebind(&s, &id)?;
    let network = resolve(&s, body.network)?;
    s.manager
        .add_nic(&id, &body.device_id, network)
        .map_err(err)?;
    // The new MAC is part of the guest's rules from now on. Refresh the guest
    // snapshot first, so the reservation exists before the rules that are keyed
    // by it are rebuilt instead of reading the 5 s-old map. No manager lock is
    // held here; a failed pass is logged and the next one settles it.
    if let Err(e) = crate::ipam::server::supervise_once(&s.ipam, &s.manager).await {
        tracing::warn!("qemu: refreshing IPAM after adding a NIC: {e}");
    }
    reapply(&s).await;
    Ok(Json(json!({"ok":true,"device_id":body.device_id})))
}
async fn remove_nic(
    State(s): State<QemuState>,
    Path((id, device_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = s.manager.remove_nic(&id, &device_id).map_err(err)?;
    if !pending {
        // The MAC is gone from the guest snapshot, so the per-VM rules can drop
        // it in the same request rather than one supervisor pass later.
        if let Err(e) = crate::ipam::server::supervise_once(&s.ipam, &s.manager).await {
            tracing::warn!("qemu: refreshing IPAM after removing a NIC: {e}");
        }
        reapply(&s).await;
    }
    Ok(Json(
        json!({"ok":!pending,"pending":pending,"device_id":device_id}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddDisk {
    id: String,
    path: std::path::PathBuf,
    #[serde(default)]
    format: Option<String>,
}

async fn add_disk(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<AddDisk>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::qemu::validate_new_disk_id(&body.id).map_err(ApiError::bad_request)?;
    let format = match body.format.as_deref().unwrap_or("raw") {
        "raw" => DiskFormat::Raw,
        "qcow2" => DiskFormat::Qcow2,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown disk format '{other}'"
            )));
        }
    };
    let path = s
        .manager
        .add_disk(&id, &body.id, &body.path, format)
        .map_err(err)?;
    Ok(Json(json!({"ok":true,"id":body.id,"path":path})))
}

async fn remove_disk(
    State(s): State<QemuState>,
    Path((id, disk_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = s.manager.remove_disk(&id, &disk_id).map_err(err)?;
    Ok(Json(
        json!({"ok":true,"id":disk_id,"device_id":disk_id,"pending":pending}),
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
        .route("/qemu/vms/{id}/state", get(state))
        .route("/qemu/vms/{id}/network", get(network).put(set_network))
        .route("/qemu/vms/{id}/firewall", get(firewall).put(set_firewall))
        .route("/qemu/vms/{id}/nics", post(add_nic))
        .route("/qemu/vms/{id}/nics/{device_id}", delete(remove_nic))
        .route("/qemu/vms/{id}/disks", post(add_disk))
        .route("/qemu/vms/{id}/disks/{disk_id}", delete(remove_disk))
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

    async fn put(app: Router, body: &str) -> (u16, serde_json::Value) {
        call(
            app,
            "PUT",
            "/qemu/vms/550e8400-e29b-41d4-a716-446655440000/network",
            body,
        )
        .await
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
        let vm = "550e8400-e29b-41d4-a716-446655440000";
        let net_id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let mac = crate::qemu::stable_mac(vm, "net0");
        s.manager
            .define(VmConfig {
                id: vm.into(),
                title: "web".into(),
                disk: disk(&dir, "disk"),
                memory_mib: 128,
                vcpus: 1,
                network: Network::Bridge {
                    network: net_id.into(),
                    bridge: "vbrstale00000".into(),
                    mac: mac.clone(),
                },
                firewall: Default::default(),
                extra_nics: vec![],
                extra_disks: vec![],
            })
            .unwrap();
        s.ipam.lock().networks.insert(
            net_id.into(),
            crate::ipam::store::import(
                "<network><title>lab</title><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
            )
            .unwrap(),
        );

        assert!(rebind(&s, vm).is_ok());
        assert_eq!(
            s.manager.list().unwrap().remove(0).network,
            Network::Bridge {
                network: net_id.into(),
                bridge: crate::hostnet::bridge_name(net_id),
                mac,
            }
        );

        // A network that has gone away is refused, not started on the old name.
        s.manager
            .set_network(
                vm,
                Network::Bridge {
                    network: "gone".into(),
                    bridge: "vbrstale00000".into(),
                    mac: String::new(),
                },
            )
            .unwrap();
        assert!(rebind(&s, vm).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_network_spec_round_trips_as_the_stored_shape() {
        let user: NetworkSpec = serde_json::from_str(r#"{"mode":"user"}"#).unwrap();
        assert!(matches!(user, NetworkSpec::User { hostfwd } if hostfwd.is_empty()));
        let bridge: NetworkSpec =
            serde_json::from_str(r#"{"mode":"bridge","network":"lab"}"#).unwrap();
        assert!(
            matches!(bridge, NetworkSpec::Bridge { network, mac } if network == "lab" && mac.is_none())
        );
        // The stored enum the GET answers with is the same tagged shape.
        assert_eq!(
            serde_json::to_value(Network::default()).unwrap(),
            serde_json::json!({"mode":"user","hostfwd":[]})
        );
        assert_eq!(
            serde_json::to_value(Network::Bridge {
                network: "lab".into(),
                bridge: "vbr0".into(),
                mac: "52:54:00:aa:bb:cc".into(),
            })
            .unwrap(),
            serde_json::json!({
                "mode":"bridge","network":"lab","bridge":"vbr0","mac":"52:54:00:aa:bb:cc"
            })
        );
    }

    /// A disk request is refused before it reaches the manager when its
    /// identifier or format is not one QEMU could be given; an unknown VM is a
    /// 404, and removal of an unknown disk likewise.
    #[tokio::test]
    async fn a_disk_request_validates_its_identifier_and_format() {
        let dir = temp("disk");
        let disk = disk(&dir, "extra");
        let vm = "550e8400-e29b-41d4-a716-446655440000";
        let path = format!("/qemu/vms/{vm}/disks");
        let body = |id: &str, format: &str| {
            serde_json::json!({"id": id, "path": disk, "format": format}).to_string()
        };
        let router = app(&dir);
        let (status, b) = call(router.clone(), "POST", &path, &body("data", "qcow3")).await;
        assert_eq!(status, 400, "{b}");
        let (status, b) = call(router.clone(), "POST", &path, &body("disk0", "qcow2")).await;
        assert_eq!(status, 400, "{b}");
        let (status, b) = call(router.clone(), "POST", &path, &body("data", "qcow2")).await;
        assert_eq!(status, 404, "{b}: no VM is defined");
        let (status, b) = call(router, "DELETE", &format!("/qemu/vms/{vm}/disks/data"), "").await;
        assert_eq!(status, 404, "{b}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
