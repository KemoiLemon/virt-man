use axum::Json;
use axum::Router;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use virt::domain::Domain;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;

use super::ApiError;
use crate::disks;
use crate::libvirt::{domain_state_name, parse_graphics, with_conn};

pub fn routes() -> Router {
    Router::new()
        .route("/domains", get(list).post(create))
        .route("/domains/{name}", get(get_one).delete(delete))
        .route("/domains/{name}/xml", get(get_xml).put(put_xml))
        .route("/domains/{name}/state", get(get_state))
        .route("/domains/{name}/stats", get(get_stats))
        .route("/domains/{name}/interfaces", get(get_interfaces))
        .route("/domains/{name}/graphics", get(get_graphics))
        .route("/domains/{name}/start", post(start))
        .route("/domains/{name}/shutdown", post(shutdown))
        .route("/domains/{name}/reboot", post(reboot))
        .route("/domains/{name}/destroy", post(destroy))
        .route("/domains/{name}/reset", post(reset))
        .route("/domains/{name}/rename", post(rename))
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
    let info = d.get_info().ok();
    // Display title: <title> metadata, if the guest set one.
    let mut title = None;
    if let Ok(xml) = d.get_xml_desc(0)
        && let Some(i) = xml.find("<title>")
    {
        let rest = &xml[i + 7..];
        if let Some(j) = rest.find("</title>") {
            title = Some(rest[..j].trim().to_string());
        }
    }
    json!({
        "name": d.get_name().unwrap_or_default(),
        "title": title,
        "id": d.get_id().unwrap_or(0),
        "state": state,
        "active": d.is_active().unwrap_or(false),
        "persistent": d.is_persistent().unwrap_or(false),
        "vcpus": info.as_ref().map(|i| i.nr_virt_cpu).unwrap_or(0),
        "memory_mb": info.as_ref().map(|i| i.memory / 1024).unwrap_or(0),
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
        // Decide the cleanup BEFORE undefining: the disk list comes from this
        // definition, and the in-use check must still be able to see every
        // other guest. Only the guest's own disks are candidates — an attached
        // ISO, a read-only disk, or anything another guest references is kept.
        let plan = if remove_storage {
            disks::plan_cleanup(&xml, &disks::sources_used_elsewhere(c, Some(&name)))
        } else {
            disks::Cleanup::default()
        };
        let flags = virt::sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
            | virt::sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
            | virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM;
        d.undefine_flags(flags)?;

        let mut removed = Vec::new();
        let mut failed = Vec::new();
        for src in &plan.removable {
            // Best effort: a disk may live outside any pool, or the pool may be
            // inactive. The VM is already gone either way, so a failure is
            // reported rather than fatal.
            let outcome = match src {
                disks::DiskSource::Path(path) => {
                    StorageVol::lookup_by_path(c, path).and_then(|vol| vol.delete(0))
                }
                disks::DiskSource::Volume { pool, volume } => StoragePool::lookup_by_name(c, pool)
                    .and_then(|p| StorageVol::lookup_by_name(&p, volume))
                    .and_then(|vol| vol.delete(0)),
            };
            match outcome {
                Ok(()) => removed.push(src.describe()),
                Err(e) => failed.push(json!({"disk": src.describe(), "error": e.to_string()})),
            }
        }
        Ok(json!({
            "name": name,
            "undefined": true,
            "storage_removed": remove_storage,
            // What went, what was deliberately kept and why, and what could
            // not be removed — so the UI can tell the whole truth.
            "removed": removed,
            "kept": plan.kept.iter()
                .map(|(disk, reason)| json!({"disk": disk, "reason": reason.as_str()}))
                .collect::<Vec<_>>(),
            "failed": failed,
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
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

/// Parse one `<interface>…</interface>` fragment's target dev, MAC and model.
/// These are child elements, so reading them off the opening tag (as the old
/// handler did) returned None for everything but the network name.
pub(super) fn interface_info(frag: &str) -> (Option<String>, Option<String>, Option<String>) {
    let child = |tag: &str, name: &str| -> Option<String> {
        let open = frag.find(&format!("<{tag}"))?;
        let tag_frag = &frag[open..];
        let tag_end = tag_frag.find('>')?;
        let t = &tag_frag[..=tag_end];
        let mut from = 0;
        while let Some(rel) = t[from..].find(name) {
            let j = from + rel;
            from = j + name.len();
            let r = t[from..].trim_start();
            let Some(r) = r.strip_prefix('=') else {
                continue;
            };
            let r = r.trim_start();
            let q = r.chars().next().unwrap_or(' ');
            if q != '\'' && q != '"' {
                continue;
            }
            let inner = &r[q.len_utf8()..];
            if let Some(k) = inner.find(q) {
                return Some(inner[..k].to_string());
            }
        }
        None
    };
    (
        child("target", "dev"),
        child("mac", "address"),
        child("model", "type"),
    )
}

/// Point-in-time per-interface byte counters, so the client can graph
/// tx/rx rates. `interface_stats` needs the guest's `target dev` name (not
/// the MAC), which is read from the domain XML first.
/// `<source network='...'/>` of an interface block, if it is on a network.
fn source_network(frag: &str) -> Option<String> {
    let s = &frag[frag.find("<source")?..];
    let s = &s[..s.find('>')?];
    let a = &s[s.find("network=")? + "network=".len()..];
    let q = a.chars().next()?;
    let a = &a[1..];
    Some(a[..a.find(q)?].to_string())
}

async fn get_interfaces(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        // Live XML: a running guest carries its `target dev` (e.g. vnet0),
        // which interface_stats needs; inactive XML omits it, and a shut-off
        // guest has no counters anyway.
        let xml = d.get_xml_desc(0)?;
        let mut out = Vec::new();
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<interface") {
            let block = &rest[i..];
            let Some(end) = block.find("</interface>") else {
                break;
            };
            let (dev, mac, model) = interface_info(&block[..end]);
            let network = source_network(&block[..end]);
            let (inbound, outbound) = super::nics::bandwidth_of(&block[..end]);
            let (rx_bytes, tx_bytes) = match dev.as_deref() {
                Some(target) => d
                    .interface_stats(target)
                    .map(|s| (s.rx_bytes.max(0) as u64, s.tx_bytes.max(0) as u64))
                    .unwrap_or((0, 0)),
                None => (0, 0),
            };
            out.push(json!({
                "dev": dev,
                "mac": mac,
                "model": model,
                "network": network,
                // Limits as the guest sees them: inbound is its RX.
                "rx_mbps": inbound.map(super::nics::kib_to_mbps),
                "tx_mbps": outbound.map(super::nics::kib_to_mbps),
                "rx_bytes": rx_bytes,
                "tx_bytes": tx_bytes,
            }));
            rest = &block[end + 12..];
        }
        Ok(json!({ "name": d.get_name().unwrap_or_default(), "interfaces": out }))
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

#[derive(serde::Deserialize)]
struct RenameReq {
    name: String,
}

/// Rename a domain. libvirt has no rename call, so this swaps the persistent
/// definition: undefine the old name and define the same XML under a new
/// `<name>`, keeping the UUID (and therefore disks, MACs and snapshot
/// metadata) intact. The guest must be shut off — a running domain's
/// definition is tied to the running process and cannot be renamed.
async fn rename(
    Path(name): Path<String>,
    Json(req): Json<RenameReq>,
) -> Result<Json<Value>, ApiError> {
    let new_name = req.name.trim().to_string();
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        if d.get_state()?.0 != virt::sys::VIR_DOMAIN_SHUTOFF {
            return Err(format!("rename requires '{name}' to be shut off").into());
        }
        if new_name.is_empty() {
            return Err("new name is empty".to_string().into());
        }
        if new_name == name {
            // Nothing to do; report success so the caller can treat rename as
            // idempotent rather than special-casing a same-name request.
            return Ok(json!({"name": name, "renamed": false}));
        }
        // Validate before undefining: a bad name must not cost the domain.
        if !new_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            return Err("name may only contain letters, digits, '-' '_' or '.'"
                .to_string()
                .into());
        }
        if Domain::lookup_by_name(c, &new_name).is_ok() {
            return Err(format!("a domain named '{new_name}' already exists").into());
        }

        let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        let Some(new_xml) = rename_xml(&xml, &new_name) else {
            return Err("no <name> element in domain XML".to_string().into());
        };

        // Undefine first, then define the renamed XML: both definitions share
        // one UUID, which libvirt will not let coexist. If the define fails,
        // put the old definition back so a rename can never destroy the VM.
        d.undefine_flags(0)?;
        if let Err(e) = Domain::define_xml(c, &new_xml) {
            let _ = Domain::define_xml(c, &xml); // rollback, best effort
            return Err(format!("define renamed domain failed: {e}").into());
        }
        Ok(json!({"name": new_name, "renamed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Swap the domain's `<name>` content, leaving every other byte — UUID,
/// devices, disk paths — untouched.
fn rename_xml(xml: &str, new_name: &str) -> Option<String> {
    let start = xml.find("<name")?;
    let open_end = start + xml[start..].find('>')?;
    let close = xml[open_end..].find("</name>")? + open_end;
    Some(format!(
        "{}{}{}",
        &xml[..open_end + 1],
        new_name,
        &xml[close..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_swaps_only_the_name_content() {
        let with_name = "<domain type='kvm'><name>old</name><uuid>abc</uuid><devices/></domain>";
        let out = rename_xml(with_name, "new").expect("name element present");
        assert!(out.contains("<name>new</name>"), "name swapped:\n{out}");
        // Everything else must survive byte-for-byte: the UUID is what makes
        // the renamed domain still the same VM to libvirt.
        assert!(out.contains("<uuid>abc</uuid>"), "uuid kept:\n{out}");
        assert!(out.contains("<domain type='kvm'>"), "attributes kept");
        assert_eq!(
            out.matches("<name>").count(),
            1,
            "no duplicated name element"
        );
    }

    #[test]
    fn rename_never_matches_neighbouring_name_prefixes() {
        // <hostname> and <nvram> both contain "name" but are NOT the domain
        // name element; a sloppy search would corrupt them.
        let xml = "<domain><name>old</name><devices><hostname>x</hostname></devices></domain>";
        let out = rename_xml(xml, "new").unwrap();
        assert_eq!(out.matches("<name>").count(), 1);
        assert!(
            out.contains("<hostname>x</hostname>"),
            "hostname untouched:\n{out}"
        );
        assert!(!out.contains("old"), "old name gone:\n{out}");
    }

    #[test]
    fn rename_without_a_name_element_is_none() {
        assert!(rename_xml("<domain><devices/></domain>", "x").is_none());
    }

    #[test]
    fn interface_info_reads_child_elements_not_tag_attributes() {
        let frag = "<interface type='network'>\n      <mac address='52:54:00:12:34:56'/>\n      <source network='internet'/>\n      <model type='virtio'/>\n      <target dev='vnet0'/>\n    </interface>";
        assert_eq!(
            interface_info(frag),
            (
                Some("vnet0".into()),
                Some("52:54:00:12:34:56".into()),
                Some("virtio".into()),
            )
        );
        // A shut-off guest's inactive XML has no <target dev>.
        let no_target = "<interface type='network'>\n      <mac address='52:54:00:12:34:56'/>\n      <model type='virtio'/>\n    </interface>";
        assert_eq!(interface_info(no_target).0, None);
    }
}
