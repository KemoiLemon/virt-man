use axum::Json;
use axum::Router;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use virt::domain::Domain;

use super::ApiError;
use crate::libvirt::{domain_state_name, parse_graphics, with_conn};

pub fn routes() -> Router {
    Router::new()
        .route("/domains", get(list).post(create))
        .route("/domains/{name}", get(get_one).delete(delete))
        .route("/domains/{name}/xml", get(get_xml).put(put_xml))
        .route("/domains/{name}/state", get(get_state))
        .route("/domains/{name}/stats", get(get_stats))
        .route("/domains/{name}/graphics", get(get_graphics))
        .route("/domains/{name}/start", post(start))
        .route("/domains/{name}/shutdown", post(shutdown))
        .route("/domains/{name}/reboot", post(reboot))
        .route("/domains/{name}/destroy", post(destroy))
        .route("/domains/{name}/reset", post(reset))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let doms = c.list_all_domains(0)?;
        let mut arr = Vec::with_capacity(doms.len());
        for d in &doms {
            arr.push(domain_summary(d));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn domain_summary(d: &Domain) -> Value {
    let state = d
        .get_state()
        .map(|(s, _)| domain_state_name(s))
        .unwrap_or("unknown");
    json!({
        "name": d.get_name().unwrap_or_default(),
        "id": d.get_id().unwrap_or(0),
        "state": state,
        "active": d.is_active().unwrap_or(false),
        "persistent": d.is_persistent().unwrap_or(false),
    })
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        Ok(json!({
            "name": d.get_name().unwrap_or_default(),
            "uuid": d.get_uuid_string().unwrap_or_default(),
            "id": d.get_id().unwrap_or(0),
            "state": d.get_state().map(|(s, _)| domain_state_name(s)).unwrap_or("unknown"),
            "active": d.is_active().unwrap_or(false),
            "persistent": d.is_persistent().unwrap_or(false),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        Ok(json!({"name": d.get_name().unwrap_or_default(), "xml": d.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let _ = Domain::define_xml(c, &body)?;
        Ok(json!({"name": name, "defined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn create(
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let start = q.get("start").map(|v| v == "true").unwrap_or(true);
    let v = with_conn(move |c| {
        let d = Domain::define_xml(c, &body)?;
        let name = d.get_name().unwrap_or_default();
        let started = if start {
            d.create().map(|_| true).unwrap_or(false)
        } else {
            false
        };
        Ok(json!({"name": name, "started": started}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let remove_storage = q
        .get("remove_storage")
        .map(|v| v == "true")
        .unwrap_or(false);
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let xml = d.get_xml_desc(0)?;
        let flags = virt::sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
            | virt::sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
            | virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM;
        d.undefine_flags(flags)?;
        if remove_storage {
            for path in disk_file_paths(&xml) {
                // Best effort: a disk may live outside any pool.
                let _ = virt::storage_vol::StorageVol::lookup_by_path(c, &path)
                    .and_then(|vol| vol.delete(0));
            }
        }
        Ok(json!({"name": name, "undefined": true, "storage_removed": remove_storage}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn disk_file_paths(xml: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<disk") {
        let block = &rest[i..];
        let is_file_disk = block[..block.len().min(400)].contains("type='file'");
        if is_file_disk {
            let src = block.find("<source>");
            if let Some(p) = src.and_then(|s| attr(&block[s..], "file")) {
                paths.push(p.to_string());
            }
        }
        rest = &block[5..];
    }
    paths
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let i = tag.find(name)?;
    let rest = tag[i + name.len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = rest.chars().next()?;
    let inner = rest.strip_prefix(quote)?;
    let j = inner.find(quote)?;
    Some(&inner[..j])
}

async fn get_state(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let (state, reason) = d.get_state()?;
        Ok(json!({"name": d.get_name().unwrap_or_default(), "state": domain_state_name(state), "reason": reason}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Point-in-time CPU/memory sample for one domain, plus the host node info
/// the caller needs to turn `cpu_time` into a percentage.
///
/// `cpu_time` is a monotonic nanosecond counter, so a single reading cannot
/// express "CPU %" — the client keeps the previous sample and divides the
/// delta by elapsed wall time and the vCPU count.
async fn get_stats(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let info = d.get_info()?;
        let node = c.get_node_info()?;
        // Balloon stats are only available for running guests with a working
        // agent/balloon driver; absence is normal, so never fail on it.
        let mut rss_kib: Option<u64> = None;
        let mut usable_kib: Option<u64> = None;
        if let Ok(stats) = d.memory_stats(0) {
            for s in stats {
                match s.tag {
                    // VIR_DOMAIN_MEMORY_STAT_RSS
                    7 => rss_kib = Some(s.val),
                    // VIR_DOMAIN_MEMORY_STAT_USABLE
                    8 => usable_kib = Some(s.val),
                    _ => {}
                }
            }
        }
        Ok(json!({
            "name": d.get_name().unwrap_or_default(),
            "state": domain_state_name(info.state),
            "cpu_time_ns": info.cpu_time,
            "vcpus": info.nr_virt_cpu,
            "memory_kib": info.memory,
            "max_memory_kib": info.max_mem,
            "rss_kib": rss_kib,
            "usable_kib": usable_kib,
            "host": {
                "cpus": node.cpus,
                "memory_kib": node.memory,
                "mhz": node.mhz,
                "model": node.model,
            },
            "sampled_at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn get_graphics(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        let xml = d.get_xml_desc(0)?;
        Ok(json!({"name": d.get_name().unwrap_or_default(), "graphics": parse_graphics(&xml)}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        d.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn shutdown(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        d.shutdown()?;
        Ok(json!({"name": name, "shutdown": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn reboot(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        d.reboot(0)?;
        Ok(json!({"name": name, "rebooted": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn destroy(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        d.destroy()?;
        Ok(json!({"name": name, "destroyed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn reset(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        d.reset()?;
        Ok(json!({"name": name, "reset": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}
