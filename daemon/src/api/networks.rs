use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};
use serde_json::{Value, json};
use virt::network::Network;

use super::ApiError;
use crate::libvirt::with_conn;

pub fn routes() -> Router {
    Router::new()
        .route("/networks", get(list).post(create))
        .route("/networks/{name}", get(get_one).delete(delete))
        .route("/networks/{name}/xml", get(get_xml).put(put_xml))
        .route("/networks/{name}/start", post(start))
        .route("/networks/{name}/stop", post(stop))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let nets = c.list_all_networks(0)?;
        let mut arr = Vec::with_capacity(nets.len());
        for n in &nets {
            arr.push(json!({
                "name": n.get_name().unwrap_or_default(),
                "bridge": n.get_bridge_name().unwrap_or_default(),
                "active": n.is_active().unwrap_or(false),
                "persistent": n.is_persistent().unwrap_or(false),
            }));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        Ok(json!({
            "name": n.get_name().unwrap_or_default(),
            "bridge": n.get_bridge_name().unwrap_or_default(),
            "active": n.is_active().unwrap_or(false),
            "persistent": n.is_persistent().unwrap_or(false),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
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

async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let _ = Network::define_xml(c, &body)?;
        Ok(json!({"name": name, "defined": true}))
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
