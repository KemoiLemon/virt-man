use axum::Json;
use axum::Router;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
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
        .route("/pools/{name}/title", axum::routing::put(set_title))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let pools = c.list_all_storage_pools(0)?;
        let mut arr = Vec::with_capacity(pools.len());
        for p in &pools {
            arr.push(pool_json(p));
        }
        // Forget display names of pools removed outside this daemon.
        let live: Vec<String> = pools
            .iter()
            .filter_map(|p| p.get_uuid_string().ok())
            .collect();
        if let Err(e) = crate::titles::retain(&live) {
            tracing::warn!("pool titles: {e}");
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn pool_json(p: &StoragePool) -> Value {
    let info = p.get_info().ok();
    let uuid = p.get_uuid_string().unwrap_or_default();
    json!({
        "name": p.get_name().unwrap_or_default(),
        "uuid": uuid,
        // libvirt has no <title> for a pool; its display name is kept here.
        "title": crate::titles::get(&uuid),
        "active": p.is_active().unwrap_or(false),
        "persistent": p.is_persistent().unwrap_or(false),
        "capacity": info.as_ref().map(|i| i.capacity),
        "allocation": info.as_ref().map(|i| i.allocation),
        "available": info.as_ref().map(|i| i.available),
    })
}

/// Bring a pool's cached state up to date before reporting it.
///
/// libvirt caches a pool's volume list and its space figures, and re-reads them
/// when the volume API changes something — `virStorageVolCreateXML` or
/// `StorageVol::delete` already leave the pool correct. What it cannot see is a
/// file that arrives any other way, which is the common case here: an ISO
/// dropped in by aria2, `cp` or `scp`, or one removed with `rm`. Those stay
/// invisible, and the space figures stay stale, until something refreshes the
/// pool — so a pool page would poll every second and show the same wrong answer
/// forever.
///
/// Measured on this host, the refresh itself is free next to the request that
/// carries it: a pool with 24 volumes and one with none both answer in ~44 ms,
/// because that time is the libvirt connection, not the directory scan. So it is
/// cheaper to be accurate than to cache.
///
/// Best effort on purpose: a pool that cannot be refreshed (inactive,
/// unreadable) should still be reported, from whatever state it has.
///
/// ponytail: rescans the pool's directory on every read, so a pool holding a
/// very large number of files pays a stat per file per poll. Rate-limit against
/// the pool's own refresh interval if that ever shows up in a profile.
pub(crate) fn refresh_for_read(p: &StoragePool) {
    let _ = p.refresh(0);
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
        refresh_for_read(&p);
        Ok(pool_json(&p))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
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

/// Define and start a pool. `?title=` sets its display name, which a pool
/// definition cannot carry (see `titles`).
async fn create(
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let title = q.get("title").cloned().unwrap_or_default();
    let v = with_conn(move |c| {
        let p = StoragePool::define_xml(c, &body, 0)?;
        let name = p.get_name().unwrap_or_default();
        let uuid = p.get_uuid_string().unwrap_or_default();
        if !title.trim().is_empty() {
            crate::titles::set(&uuid, &title)?;
        }
        p.build(0).ok();
        p.create(0)?;
        p.set_autostart(true)?;
        Ok(json!({"name": name, "uuid": uuid, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
        if p.is_active().unwrap_or(false) {
            p.destroy()?;
        }
        let uuid = p.get_uuid_string().unwrap_or_default();
        p.undefine()?;
        crate::titles::set(&uuid, "")?;
        Ok(json!({"name": name, "undefined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
        p.create(0)?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn stop(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
        p.destroy()?;
        Ok(json!({"name": name, "stopped": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn refresh(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let p = crate::libvirt::pool(c, &name)?;
        p.refresh(0)?;
        Ok(json!({"name": name, "refreshed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(serde::Deserialize)]
struct TitleReq {
    title: String,
}

/// Rename a pool as people see it. Its libvirt name (its UUID) stays put, so
/// no guest's disk reference changes.
async fn set_title(
    Path(name): Path<String>,
    Json(req): Json<TitleReq>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let uuid = crate::libvirt::pool(c, &name)?.get_uuid_string()?;
        crate::titles::set(&uuid, &req.title)?;
        Ok(json!({"uuid": uuid, "title": req.title.trim()}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}
