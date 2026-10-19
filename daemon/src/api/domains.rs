use axum::Json;
use axum::Router;
use axum::extract::{Extension, Path, Query};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use virt::domain::Domain;

use super::ApiError;
use crate::disks;
use crate::ipam::server::Ipam;
use crate::libvirt::{domain_state_name, parse_graphics, with_conn};
use crate::pools::Pools;

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
    let title = d
        .get_xml_desc(0)
        .ok()
        .and_then(|x| crate::libvirt::title_of(&x));
    json!({
        "name": d.get_name().unwrap_or_default(),
        "uuid": d.get_uuid_string().unwrap_or_default(),
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
        let d = crate::libvirt::domain(c, &name)?;
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
        let d = crate::libvirt::domain(c, &name)?;
        Ok(json!({"name": d.get_name().unwrap_or_default(), "xml": d.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        // Resolve OVMF paths the forms wrote for another distribution to ones
        // that exist here, or a Debian host refuses a guest the Arch-flavoured
        // form produced (and the reverse).
        let _ = Domain::define_xml(c, &crate::firmware::resolve(&body))?;
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
        let d = Domain::define_xml(c, &crate::firmware::resolve(&body))?;
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
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let remove_storage = q
        .get("remove_storage")
        .map(|v| v == "true")
        .unwrap_or(false);
    // Stop a running guest before removing it. Undefining a live domain leaves
    // it running and transient -- an orphan with no definition to manage -- so
    // a delete asks the guest to shut down, waits (bounded), then forces it off
    // and undefines. A delete therefore always ends with the guest gone.
    let stopped = crate::libvirt::stop_guest(&name, std::time::Duration::from_secs(30))
        .await
        .map_err(ApiError::internal)?;
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        let xml = d.get_xml_desc(0)?;
        // Decide the cleanup BEFORE undefining: the disk list comes from this
        // definition, and the in-use check must still be able to see every
        // other guest. Only the guest's own disks are candidates — an attached
        // ISO, a read-only disk, or anything another guest references is kept.
        let plan = if remove_storage {
            disks::plan_cleanup_connected(
                &pools,
                &xml,
                &disks::sources_used_elsewhere(c, &pools, Some(&name)),
            )
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
            // Best effort: only a file inside one of our pools is removed, so
            // a definition naming a host path (an ISO elsewhere, /etc/shadow)
            // can never make a guest delete reach it.
            let outcome: Result<(), String> = match src {
                disks::DiskSource::Path(path) => pools
                    .owned_file(path)
                    .ok_or_else(|| "not a file in a storage pool".to_string())
                    .and_then(|p| std::fs::remove_file(p).map_err(|e| e.to_string())),
                disks::DiskSource::Volume { pool, volume } => pools
                    .volume_path(pool, volume)
                    .ok_or_else(|| format!("no volume '{volume}' in pool '{pool}'"))
                    .and_then(|p| std::fs::remove_file(p).map_err(|e| e.to_string())),
            };
            match outcome {
                Ok(()) => removed.push(src.describe()),
                Err(e) => failed.push(json!({"disk": src.describe(), "error": e})),
            }
        }
        Ok(json!({
            "name": name,
            "undefined": true,
            // Whether the guest had to be stopped first, so the UI can say so.
            "stopped": stopped,
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
        let d = crate::libvirt::domain(c, &name)?;
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
        let d = crate::libvirt::domain(c, &name)?;
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
/// The `<source ...>` tag of an interface block, if it has one.
fn source_tag(frag: &str) -> Option<&str> {
    let s = &frag[frag.find("<source")?..];
    Some(&s[..s.find('>')?])
}

/// `<source network='...'/>` of an interface block, if it is on a libvirt
/// network. Only a definition written before networks became our own has one.
fn source_network(frag: &str) -> Option<String> {
    disks::attr(source_tag(frag)?, "network").map(str::to_string)
}

/// `<source bridge='...'/>` of an interface block: the bridge the guest is
/// plugged into, which is how a network is attached now.
fn source_bridge(frag: &str) -> Option<String> {
    disks::attr(source_tag(frag)?, "bridge").map(str::to_string)
}

async fn get_interfaces(
    Extension(ipam): Extension<Ipam>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
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
            let b = &block[..end];
            let (dev, mac, model) = interface_info(b);
            // The network key, not the bridge: the VM's Network tab and the
            // IPAM table are keyed by it. A bridge nothing here defines is an
            // operator's own, and has no network key to report.
            let open = &b[..b.find('>').map_or(b.len(), |e| e + 1)];
            let network = if disks::attr(open, "type") == Some("bridge") {
                source_bridge(b).and_then(|br| ipam.network_for_bridge(&br))
            } else {
                source_network(b)
            };
            let (inbound, outbound) = super::nics::bandwidth_of(b);
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
        let d = crate::libvirt::domain(c, &name)?;
        let xml = d.get_xml_desc(0)?;
        Ok(json!({"name": d.get_name().unwrap_or_default(), "graphics": parse_graphics(&xml)}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        d.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn shutdown(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        d.shutdown()?;
        Ok(json!({"name": name, "shutdown": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn reboot(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        d.reboot(0)?;
        Ok(json!({"name": name, "rebooted": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn destroy(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        d.destroy()?;
        Ok(json!({"name": name, "destroyed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn reset(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
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
        let d = crate::libvirt::domain(c, &name)?;
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

        // libvirt's own rename, not undefine + define. Undefining a UEFI guest
        // with no flags is refused outright ("Cannot undefine domain with
        // NVRAM/varstore", measured), so the old way could not rename any UEFI
        // guest -- every guest the forms make. virDomainRename keeps the UUID,
        // the variable store and everything else, in one step, and fails
        // without having changed anything.
        d.rename(&new_name, 0)?;
        Ok(json!({"name": new_name, "renamed": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renaming a UEFI guest keeps its identity and its variable store. The
    /// old undefine + define could not: libvirt refuses to undefine a guest
    /// with a varstore unless told what to do with it, so it renamed no UEFI
    /// guest at all. Real libvirt, one throwaway guest that is never started.
    /// `sudo -E cargo test --bins -- --ignored renaming_a_uefi`
    #[tokio::test]
    #[ignore = "needs root and a reachable qemu:///system"]
    async fn renaming_a_uefi_guest_keeps_its_uuid_and_varstore() {
        const NV: &str = "/var/lib/libvirt/qemu/nvram/renameprobe_VARS.fd";
        let xml = format!(
            "<domain type='kvm'><name>renameprobe</name><memory unit='KiB'>131072</memory><vcpu>1</vcpu>\
             <os><type arch='x86_64' machine='q35'>hvm</type>\
             <loader readonly='yes' type='pflash' format='raw'>/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>\
             <nvram template='/usr/share/edk2/x64/OVMF_VARS.4m.fd' format='raw'>{NV}</nvram></os>\
             <features><acpi/></features><devices/></domain>"
        );
        let uuid = with_conn(move |c| Ok(Domain::define_xml(c, &xml)?.get_uuid_string()?))
            .await
            .expect("define");
        std::fs::write(NV, b"probe-vars").expect("varstore");

        let res = rename(
            Path("renameprobe".into()),
            Json(RenameReq {
                name: "renameprobe-2".into(),
            }),
        )
        .await;
        let u2 = uuid.clone();
        let after = with_conn(move |c| {
            let d = crate::libvirt::domain(c, &u2)?;
            Ok((d.get_name()?, d.get_xml_desc(0)?))
        })
        .await;
        let vars = std::fs::read(NV).ok();
        let u3 = uuid.clone();
        let _ = with_conn(move |c| {
            crate::libvirt::domain(c, &u3)?.undefine_flags(virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM)?;
            Ok(())
        })
        .await;

        assert!(
            res.is_ok(),
            "rename refused: {:?}",
            res.err().map(|e| e.message)
        );
        let (name, xml) = after.expect("found by uuid after the rename");
        assert_eq!(name, "renameprobe-2", "renamed, same UUID");
        assert!(xml.contains(NV), "the varstore is still attached");
        assert_eq!(
            vars.as_deref(),
            Some(&b"probe-vars"[..]),
            "and its contents kept"
        );
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
    fn an_interfaces_source_is_read_for_both_attachment_shapes() {
        // What this daemon writes: the network's bridge.
        let bridged =
            "<interface type='bridge'>\n      <source bridge='vbr0123456789ab'/>\n    </interface>";
        assert_eq!(source_bridge(bridged).as_deref(), Some("vbr0123456789ab"));
        assert_eq!(source_network(bridged), None, "a bridge is not a network");
        // An older definition still names a libvirt network.
        let networked =
            "<interface type='network'>\n      <source network='lab'/>\n    </interface>";
        assert_eq!(source_network(networked).as_deref(), Some("lab"));
        assert_eq!(source_bridge(networked), None);
        // libvirt adds the resolved bridge to a running network NIC's live XML;
        // it is informational, and the network is still what is read.
        let live = "<interface type='network'><source network='lab' bridge='virbr0'/></interface>";
        assert_eq!(source_network(live).as_deref(), Some("lab"));
        // A NIC with no source, or no interface at all, reports neither.
        assert_eq!(source_network("<interface type='bridge'/>"), None);
        assert_eq!(source_bridge("<interface type='bridge'/>"), None);
    }

    /// Deleting a *running* guest stops it first.
    ///
    /// Undefining a live domain leaves it running and transient -- an orphan
    /// with no definition to manage it by -- which is what this guards against.
    /// A tiny direct-kernel guest boots and is deleted while active; the test
    /// checks it was reported `stopped`, and that neither its definition nor a
    /// running domain survives. Real libvirt; never leaves anything behind.
    /// `sudo -E cargo test --bins -- --ignored deleting_a_running`
    #[tokio::test]
    #[ignore = "needs root and a reachable qemu:///system"]
    async fn deleting_a_running_guest_stops_it_first() {
        use std::collections::HashMap;
        // A guest that cannot start without a bootable kernel is no use here, so
        // use the host kernel. If there is none, skip rather than fail.
        let kernel = [
            "/boot/vmlinuz-linux",
            "/boot/vmlinuz",
            "/boot/vmlinuz-linux-lts",
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
        let Some(kernel) = kernel else {
            eprintln!("skipping: no /boot kernel to boot the probe guest from");
            return;
        };
        let xml = format!(
            "<domain type='kvm'><name>delproberun</name><memory unit='KiB'>262144</memory><vcpu>1</vcpu>\
             <os><type arch='x86_64' machine='q35'>hvm</type>\
             <kernel>{kernel}</kernel><cmdline>console=ttyS0 quiet</cmdline></os>\
             <features><acpi/></features>\
             <devices><serial type='pty'><target port='0'/></serial></devices></domain>"
        );
        with_conn({
            let xml = xml.clone();
            move |c| {
                // Clean up any leftover from an interrupted run.
                if let Ok(d) = crate::libvirt::domain(c, "delproberun") {
                    let _ = d.destroy();
                    let _ = d.undefine_flags(
                        virt::sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
                            | virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM,
                    );
                }
                Domain::define_xml(c, &xml)?;
                Ok(())
            }
        })
        .await
        .expect("define probe");
        with_conn(|c| {
            crate::libvirt::domain(c, "delproberun")?.create()?;
            Ok(())
        })
        .await
        .expect("start probe");

        // Delete it while it is running. The handler must stop it first.
        let scratch = std::env::temp_dir().join(format!("delprobe-pools-{}", std::process::id()));
        let pools = crate::pools::Pools::load(scratch.join("pools.json")).expect("pools store");
        let res = match delete(
            Extension(pools),
            Path("delproberun".into()),
            Query(HashMap::new()),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                // Clean up before failing so a broken run leaves nothing.
                let _ = with_conn(|c| {
                    if let Ok(d) = crate::libvirt::domain(c, "delproberun") {
                        let _ = d.destroy();
                        let _ = d.undefine_flags(
                            virt::sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
                                | virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM,
                        );
                    }
                    Ok(())
                })
                .await;
                panic!("delete refused: {}", e.message);
            }
        };
        let stopped = res.0["stopped"].as_bool().unwrap_or(false);
        let undefined = res.0["undefined"].as_bool().unwrap_or(false);

        // After the delete: the domain must be gone, not left running.
        let survives = with_conn(|c| {
            Ok(match crate::libvirt::domain(c, "delproberun") {
                Ok(d) => (true, d.is_active().unwrap_or(false)),
                Err(_) => (false, false),
            })
        })
        .await
        .expect("probe after delete");

        assert!(stopped, "the delete reported stopping the running guest");
        assert!(undefined, "the delete undefined the guest");
        assert!(
            !survives.0,
            "no definition survives a running guest's delete"
        );
        assert!(!survives.1, "and no running domain is left behind");
    }
}
