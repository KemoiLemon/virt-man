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
        .route(
            "/domains/{name}/firewall",
            get(get_firewall).put(put_firewall),
        )
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

/// Per-interface firewall state: which nwfilter (if any) each NIC references.
async fn get_firewall(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        // INACTIVE: a running guest's live XML omits a filterref added since
        // boot, so reading it would report "no filter" for one that is
        // configured and will apply on next start.
        let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        Ok(json!({
            "name": d.get_name().unwrap_or_default(),
            "interfaces": interface_filters(&xml),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(serde::Deserialize)]
struct FirewallReq {
    /// nwfilter name to apply to every network interface, or null to clear.
    filter: Option<String>,
}

/// Apply (or clear) an nwfilter on all of the domain's network interfaces.
///
/// The change is written to the persistent definition, so it takes effect the
/// next time the guest starts.
async fn put_firewall(
    Path(name): Path<String>,
    Json(req): Json<FirewallReq>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let d = Domain::lookup_by_name(c, &name)?;
        // Edit the persistent definition, not the running guest's live XML
        // (which carries runtime-only elements like <target dev='vnet3'/>).
        let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        let updated = set_interface_filters(&xml, req.filter.as_deref());
        Domain::define_xml(c, &updated)?;
        Ok(json!({
            "name": name,
            "filter": req.filter,
            "interfaces": interface_filters(&updated),
        }))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Per-interface `(mac, network, filter)` triples parsed from domain XML.
fn interface_filters(xml: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<interface") {
        let block = &rest[i..];
        let end = match block.find("</interface>") {
            Some(e) => e,
            None => break,
        };
        let iface = &block[..end];
        let tag_of = |needle: &str| -> Option<&str> {
            let j = iface.find(needle)?;
            let t = &iface[j..];
            let k = t.find('>')?;
            Some(&t[..=k])
        };
        out.push(json!({
            "mac": tag_of("<mac").and_then(|t| attr(t, "address")).unwrap_or_default(),
            "network": tag_of("<source").and_then(|t| attr(t, "network")).unwrap_or_default(),
            // `attr(tag, "filter")` would match inside the tag NAME
            // (`<filterref`) and then fail, so search past the tag name.
            "filter": tag_of("<filterref")
                .and_then(|t| t.strip_prefix("<filterref"))
                .and_then(|t| attr(t, "filter")),
        }));
        rest = &block[end..];
    }
    out
}

/// Rewrite every `<interface>` so it references `filter`, or none when null.
fn set_interface_filters(xml: &str, filter: Option<&str>) -> String {
    let mut out = String::with_capacity(xml.len() + 128);
    let mut rest = xml;
    while let Some(i) = rest.find("<interface") {
        let (before, block) = rest.split_at(i);
        out.push_str(before);
        let end = match block.find("</interface>") {
            Some(e) => e,
            None => break,
        };
        let mut iface = block[..end].to_string();

        // Drop any existing filterref, including a nested (non self-closing)
        // one carrying parameters.
        if let Some(f) = iface.find("<filterref") {
            let tail = &iface[f..];
            let cut = if let Some(close) = tail.find("</filterref>") {
                close + "</filterref>".len()
            } else {
                tail.find("/>").map(|e| e + 2).unwrap_or(0)
            };
            iface.replace_range(f..f + cut, "");
        }

        if let Some(name) = filter {
            // Insert just before the closing tag of the interface block.
            iface.push_str(&format!("  <filterref filter='{name}'/>\n    "));
        }
        out.push_str(&iface);
        out.push_str("</interface>");
        rest = &block[end + "</interface>".len()..];
    }
    out.push_str(rest);
    out
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
    Some(format!("{}{}{}", &xml[..open_end + 1], new_name, &xml[close..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape libvirt actually emits, including the PCI address and an
    /// unrelated <interface> substring risk in neighbouring elements.
    const XML: &str = r#"<domain type='kvm'>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <interface type='network'>
      <mac address='52:54:00:3d:53:9e'/>
      <source network='internet'/>
      <model type='virtio'/>
      <address type='pci' domain='0x0000' bus='0x01' slot='0x00' function='0x0'/>
    </interface>
    <graphics type='spice' port='5900'/>
  </devices>
</domain>"#;

    #[test]
    fn applying_a_filter_preserves_the_rest_of_the_interface() {
        let out = set_interface_filters(XML, Some("clean-traffic"));
        assert!(
            out.contains("<filterref filter='clean-traffic'/>"),
            "filter added"
        );
        // Everything that was there must survive: losing any of these would
        // silently break the guest's networking.
        for keep in [
            "<mac address='52:54:00:3d:53:9e'/>",
            "<source network='internet'/>",
            "<model type='virtio'/>",
            "type='pci'",
            "<emulator>/usr/bin/qemu-system-x86_64</emulator>",
            "<graphics type='spice' port='5900'/>",
        ] {
            assert!(out.contains(keep), "lost {keep} from:\n{out}");
        }
        assert_eq!(
            out.matches("<interface").count(),
            1,
            "no duplicated interface"
        );
        assert_eq!(out.matches("</interface>").count(), 1);
    }

    #[test]
    fn the_applied_filter_is_read_back() {
        let out = set_interface_filters(XML, Some("clean-traffic"));
        let ifaces = interface_filters(&out);
        assert_eq!(ifaces.len(), 1);
        // Regression: attr() used to match "filter" inside the tag name
        // "filterref" and return None, so the UI never saw the applied filter.
        assert_eq!(ifaces[0]["filter"], "clean-traffic");
        assert_eq!(ifaces[0]["mac"], "52:54:00:3d:53:9e");
        assert_eq!(ifaces[0]["network"], "internet");
    }

    #[test]
    fn clearing_removes_the_filter_but_keeps_the_interface() {
        let applied = set_interface_filters(XML, Some("clean-traffic"));
        let cleared = set_interface_filters(&applied, None);
        assert!(!cleared.contains("filterref"), "filter removed:\n{cleared}");
        assert!(cleared.contains("<mac address='52:54:00:3d:53:9e'/>"));
        assert!(cleared.contains("<source network='internet'/>"));
        assert_eq!(
            interface_filters(&cleared)[0]["filter"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn re_applying_does_not_stack_filterrefs() {
        let mut xml = XML.to_string();
        for _ in 0..3 {
            xml = set_interface_filters(&xml, Some("clean-traffic"));
        }
        assert_eq!(
            xml.matches("filterref").count(),
            1,
            "exactly one filterref:\n{xml}"
        );
    }

    #[test]
    fn switching_filters_replaces_rather_than_appends() {
        let a = set_interface_filters(XML, Some("clean-traffic"));
        let b = set_interface_filters(&a, Some("no-mac-spoofing"));
        assert!(b.contains("no-mac-spoofing"));
        assert!(!b.contains("clean-traffic"), "old filter removed:\n{b}");
        assert_eq!(b.matches("filterref").count(), 1);
    }

    #[test]
    fn a_nested_filterref_with_parameters_is_replaced_whole() {
        let nested = XML.replace(
            "<model type='virtio'/>",
            "<model type='virtio'/>\n      <filterref filter='clean-traffic'>\
             \n        <parameter name='IP' value='10.0.0.5'/>\n      </filterref>",
        );
        let out = set_interface_filters(&nested, Some("allow-arp"));
        assert!(
            !out.contains("parameter name='IP'"),
            "stale parameters dropped:\n{out}"
        );
        assert_eq!(out.matches("filterref").count(), 1);
        assert!(out.contains("allow-arp"));
    }

    #[test]
    fn rename_swaps_only_the_name_content() {
        let with_name = "<domain type='kvm'><name>old</name><uuid>abc</uuid><devices/></domain>";
        let out = rename_xml(with_name, "new").expect("name element present");
        assert!(out.contains("<name>new</name>"), "name swapped:\n{out}");
        // Everything else must survive byte-for-byte: the UUID is what makes
        // the renamed domain still the same VM to libvirt.
        assert!(out.contains("<uuid>abc</uuid>"), "uuid kept:\n{out}");
        assert!(out.contains("<domain type='kvm'>"), "attributes kept");
        assert_eq!(out.matches("<name>").count(), 1, "no duplicated name element");
    }

    #[test]
    fn rename_never_matches_neighbouring_name_prefixes() {
        // <hostname> and <nvram> both contain "name" but are NOT the domain
        // name element; a sloppy search would corrupt them.
        let xml = "<domain><name>old</name><devices><hostname>x</hostname></devices></domain>";
        let out = rename_xml(xml, "new").unwrap();
        assert_eq!(out.matches("<name>").count(), 1);
        assert!(out.contains("<hostname>x</hostname>"), "hostname untouched:\n{out}");
        assert!(!out.contains("old"), "old name gone:\n{out}");
    }

    #[test]
    fn rename_without_a_name_element_is_none() {
        assert!(rename_xml("<domain><devices/></domain>", "x").is_none());
    }

    #[test]
    fn a_domain_with_no_interface_is_returned_unchanged() {
        let none = "<domain><devices><emulator>/x</emulator></devices></domain>";
        assert_eq!(set_interface_filters(none, Some("clean-traffic")), none);
        assert!(interface_filters(none).is_empty());
    }

    #[test]
    fn every_interface_gets_the_filter() {
        let two = XML.replace(
            "<graphics type='spice' port='5900'/>",
            "<interface type='network'>\n      <mac address='52:54:00:aa:bb:cc'/>\
             \n      <source network='internal'/>\n    </interface>",
        );
        let out = set_interface_filters(&two, Some("clean-traffic"));
        assert_eq!(out.matches("filterref").count(), 2, "both NICs filtered");
        let ifaces = interface_filters(&out);
        assert_eq!(ifaces.len(), 2);
        assert!(ifaces.iter().all(|i| i["filter"] == "clean-traffic"));
    }
}
