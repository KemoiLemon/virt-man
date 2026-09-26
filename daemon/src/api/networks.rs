use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use virt::network::Network;

use super::ApiError;
use crate::libvirt::with_conn;

pub fn routes() -> Router {
    Router::new()
        .route("/networks", get(list).post(create))
        .route("/networks/bridges", get(bridges))
        .route("/networks/{name}", get(get_one).delete(delete))
        .route("/networks/{name}/xml", get(get_xml).put(put_xml))
        .route("/networks/{name}/start", post(start))
        .route("/networks/{name}/stop", post(stop))
        .route("/networks/{name}/autostart", post(set_autostart))
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
    json!({
        "name": n.get_name().unwrap_or_default(),
        "bridge": n.get_bridge_name().unwrap_or_default(),
        "active": n.is_active().unwrap_or(false),
        "persistent": n.is_persistent().unwrap_or(false),
        "autostart": n.get_autostart().unwrap_or(false),
    })
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
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

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        Ok(json!({"name": n.get_name().unwrap_or_default(), "xml": n.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Define/replace a network. The edited XML carries the original uuid, so
/// define_xml updates it in place; a name change needs the network inactive
/// first, so destroy a running one and restart it after the define.
async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let was_active = Network::lookup_by_name(c, &name)
            .map(|n| n.is_active().unwrap_or(false))
            .unwrap_or(false);
        if was_active && let Ok(n) = Network::lookup_by_name(c, &name) {
            n.destroy()?;
        }
        let n = Network::define_xml(c, &body)?;
        let new_name = n.get_name().unwrap_or_default();
        let started = if was_active {
            n.create().map(|_| true).unwrap_or(false)
        } else {
            false
        };
        Ok(json!({"name": new_name, "defined": true, "started": started}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn create(body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::define_xml(c, &body)?;
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
        let n = Network::lookup_by_name(c, &name)?;
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
        let n = Network::lookup_by_name(c, &name)?;
        n.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn stop(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
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
        let n = Network::lookup_by_name(c, &name)?;
        n.set_autostart(req.autostart)?;
        Ok(json!({"name": name, "autostart": req.autostart}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}
