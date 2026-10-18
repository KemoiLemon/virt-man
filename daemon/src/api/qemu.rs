use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use crate::qemu::{Manager, State as VmState, VmConfig};

#[derive(Clone)]
struct QemuState(Manager);

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
    disk: std::path::PathBuf,
    memory_mib: u32,
    vcpus: u16,
}
#[derive(Deserialize)]
struct SetNetwork {
    network: crate::qemu_network::UserNetwork,
}

async fn list(State(s): State<QemuState>) -> Result<Json<Vec<VmConfig>>, ApiError> {
    s.0.list().map(Json).map_err(ApiError::internal)
}
async fn define(
    State(s): State<QemuState>,
    Json(body): Json<Define>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.define(VmConfig {
        id: body.id,
        disk: body.disk,
        memory_mib: body.memory_mib,
        vcpus: body.vcpus,
        network: Default::default(),
        firewall: Default::default(),
        extra_nics: vec![],
    })
    .map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn firewall(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config =
        s.0.list()
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
    s.0.set_firewall(&id, policy.clone()).map_err(err)?;
    Ok(Json(
        json!({"policy":"hostfwd_allowlist", "guest_filtering":"unsupported", "egress_filtering":"unsupported", "rules":policy.hostfwd}),
    ))
}
async fn network(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config =
        s.0.list()
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| ApiError::not_found("unknown QEMU VM"))?;
    Ok(Json(
        json!({"mode":"user_mode_nat", "bridge_available":false, "network":config.network}),
    ))
}
async fn state(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let state = s.0.state(&id).map_err(err)?;
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
    Json(body): Json<SetNetwork>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Legacy hostfwd input is rejected: inbound policy has its own admin-only endpoint.
    if !body.network.hostfwd.is_empty() {
        return Err(ApiError::bad_request(
            "hostfwd must be configured through the firewall allowlist",
        ));
    }
    s.0.set_network(&id, body.network).map_err(err)?;
    Ok(Json(
        json!({"ok": true, "network": "user_mode_nat", "bridge": "unavailable"}),
    ))
}
#[derive(Deserialize)]
struct Nic {
    device_id: String,
}
async fn add_nic(
    State(s): State<QemuState>,
    Path(id): Path<String>,
    Json(body): Json<Nic>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.add_nic(&id, &body.device_id).map_err(err)?;
    Ok(Json(json!({"ok":true,"device_id":body.device_id})))
}
async fn remove_nic(
    State(s): State<QemuState>,
    Path((id, device_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = s.0.remove_nic(&id, &device_id).map_err(err)?;
    Ok(Json(
        json!({"ok":!pending,"pending":pending,"device_id":device_id}),
    ))
}
async fn start(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.start(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn shutdown(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.powerdown(&id, false).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn force_stop(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.force_stop(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}
async fn remove(
    State(s): State<QemuState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.0.delete(&id).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}

pub fn routes(manager: Manager) -> Router {
    Router::new()
        .route("/qemu/vms", get(list).post(define))
        .route("/qemu/vms/{id}", delete(remove))
        .route("/qemu/vms/{id}/state", get(state))
        .route("/qemu/vms/{id}/network", get(network).put(set_network))
        .route("/qemu/vms/{id}/firewall", get(firewall).put(set_firewall))
        .route("/qemu/vms/{id}/nics", post(add_nic))
        .route("/qemu/vms/{id}/nics/{device_id}", delete(remove_nic))
        .route("/qemu/vms/{id}/start", post(start))
        .route("/qemu/vms/{id}/shutdown", post(shutdown))
        .route("/qemu/vms/{id}/force-stop", post(force_stop))
        .with_state(QemuState(manager))
}
