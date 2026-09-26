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
    json!({
        "name": d.get_name().unwrap_or_default(),
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
        let flags = virt::sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
            | virt::sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
            | virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM;
        d.undefine_flags(flags)?;
        if remove_storage {
            for src in disk_sources(&xml) {
                // Best effort: a disk may live outside any pool, or the pool
                // may be inactive. The VM is already gone either way.
                match src {
                    DiskSource::Path(path) => {
                        let _ = virt::storage_vol::StorageVol::lookup_by_path(c, &path)
                            .and_then(|vol| vol.delete(0));
                    }
                    DiskSource::Volume { pool, volume } => {
                        let _ = virt::storage_pool::StoragePool::lookup_by_name(c, &pool)
                            .and_then(|p| {
                                virt::storage_vol::StorageVol::lookup_by_name(&p, &volume)
                            })
                            .and_then(|vol| vol.delete(0));
                    }
                }
            }
        }
        Ok(json!({ "name": name, "undefined": true, "storage_removed": remove_storage }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// A disk's backing store, from the domain XML.
#[derive(Debug, PartialEq)]
enum DiskSource {
    /// File-backed disk: delete by absolute path.
    Path(String),
    /// Volume-backed disk: delete by pool + volume name.
    Volume { pool: String, volume: String },
}

/// Extract every disk's backing store. Handles the volume-backed disks this
/// UI creates (`<source pool=… volume=…/>`) as well as file-backed ones; the
/// old path-only parser searched for `<source>` (with no attributes) and so
/// matched nothing, leaving disks behind on delete.
fn disk_sources(xml: &str) -> Vec<DiskSource> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<disk") {
        let block = &rest[i..];
        let Some(end) = block.find("</disk>") else {
            break;
        };
        let disk = &block[..end];
        if let Some(si) = disk.find("<source") {
            let source = &disk[si..];
            // Up to the closing '>' of the <source …/> element, so attr()
            // reads attributes from the right tag even when self-closing.
            let src_end = source.find('>').map(|e| &source[..=e]).unwrap_or(source);
            if let Some(path) = attr(src_end, "file") {
                out.push(DiskSource::Path(path.to_string()));
            } else if let (Some(pool), Some(volume)) =
                (attr(src_end, "pool"), attr(src_end, "volume"))
            {
                out.push(DiskSource::Volume {
                    pool: pool.to_string(),
                    volume: volume.to_string(),
                });
            }
        }
        rest = &block[end + 7..];
    }
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    // Match the attribute name as a whole token, not a substring: a volume
    // named "some-file.iso" must not satisfy a search for "file".
    let mut start = 0;
    while let Some(rel) = tag[start..].find(name) {
        let i = start + rel;
        let before_ok = tag[..i]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace());
        if before_ok {
            let rest = tag[i + name.len()..].trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next()?;
                let inner = rest.strip_prefix(quote)?;
                let j = inner.find(quote)?;
                return Some(&inner[..j]);
            }
        }
        start = i + name.len();
    }
    None
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
fn interface_info(frag: &str) -> (Option<String>, Option<String>, Option<String>) {
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

    #[test]
    fn disk_sources_reads_volume_and_file_backed_disks() {
        let xml = r#"<domain><devices>
            <disk type='volume' device='disk'>
              <source pool='default' volume='Alpine.qcow2'/>
              <target dev='vda' bus='virtio'/>
            </disk>
            <disk type='file' device='disk'>
              <source file='/var/lib/libvirt/images/extra.raw'/>
              <target dev='vdb' bus='virtio'/>
            </disk>
            <disk type='file' device='cdrom'>
              <target dev='sda' bus='sata'/>
              <readonly/>
            </disk>
        </devices></domain>"#;
        assert_eq!(
            disk_sources(xml),
            vec![
                DiskSource::Volume {
                    pool: "default".into(),
                    volume: "Alpine.qcow2".into()
                },
                DiskSource::Path("/var/lib/libvirt/images/extra.raw".into()),
            ]
        );
    }

    #[test]
    fn disk_sources_never_matches_an_attribute_name_inside_a_value() {
        // A volume literally named "some-file.iso" must not register as a
        // file-backed disk.
        let xml = r#"<domain><devices>
            <disk type='volume' device='cdrom'>
              <source pool='ISOs' volume='some-file.iso'/>
              <target dev='sda' bus='sata'/>
            </disk>
        </devices></domain>"#;
        assert_eq!(
            disk_sources(xml),
            vec![DiskSource::Volume {
                pool: "ISOs".into(),
                volume: "some-file.iso".into()
            }]
        );
    }
}
