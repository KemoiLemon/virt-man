use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};
use serde_json::{Value, json};
use virt::storage_pool::StoragePool;

use super::ApiError;
use crate::libvirt::with_conn;

pub fn routes() -> Router {
    Router::new()
        .route("/pools", get(list).post(create))
        .route("/pools/{name}", get(get_one).delete(delete))
        .route("/pools/{name}/xml", get(get_xml).put(put_xml))
        .route("/pools/{name}/start", post(start))
        .route("/pools/{name}/stop", post(stop))
        .route("/pools/{name}/refresh", post(refresh))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let pools = c.list_all_storage_pools(0)?;
        let mut arr = Vec::with_capacity(pools.len());
        for p in &pools {
            arr.push(pool_json(p));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn pool_json(p: &StoragePool) -> Value {
    let info = p.get_info().ok();
    json!({
        "name": p.get_name().unwrap_or_default(),
        "active": p.is_active().unwrap_or(false),
        "persistent": p.is_persistent().unwrap_or(false),
        "capacity": info.as_ref().map(|i| i.capacity),
        "allocation": info.as_ref().map(|i| i.allocation),
        "available": info.as_ref().map(|i| i.available),
    })
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        Ok(pool_json(&p))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        Ok(json!({"name": p.get_name().unwrap_or_default(), "xml": p.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let _ = StoragePool::define_xml(c, &body, 0)?;
        Ok(json!({"name": name, "defined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn create(body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::define_xml(c, &body, 0)?;
        let name = p.get_name().unwrap_or_default();
        p.build(0).ok();
        p.create(0)?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        if p.is_active().unwrap_or(false) {
            p.destroy()?;
        }
        p.undefine()?;
        Ok(json!({"name": name, "undefined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        p.create(0)?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn stop(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        p.destroy()?;
        Ok(json!({"name": name, "stopped": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn refresh(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = StoragePool::lookup_by_name(c, &name)?;
        p.refresh(0)?;
        Ok(json!({"name": name, "refreshed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}
