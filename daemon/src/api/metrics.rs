//! Everything the web server's history collector needs, in one call.
//!
//! The collector samples every host every few seconds whether or not anyone
//! is looking, so it asks once per tick rather than walking the per-domain
//! endpoints (which would be one TLS request per VM, NIC and disk).
//!
//! Counters are reported raw, as the other stats endpoints do; the collector
//! turns deltas into rates. Every domain lists its NICs and disks from the
//! *definition* even when their counters cannot be read: the collector deletes
//! the history of anything missing from this answer, so a transient read
//! failure must not look like a removed device.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};
use virt::connect::Connect;
use virt::domain::Domain;

use super::ApiError;
use crate::disks::attr;
use crate::libvirt::{domain_state_name, with_conn};

pub fn routes() -> Router {
    Router::new().route("/metrics", get(metrics))
}

async fn metrics() -> Result<Json<Value>, ApiError> {
    let stat = std::fs::read_to_string("/proc/stat")
        .map_err(|e| ApiError::internal(format!("read /proc/stat: {e}")))?;
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|e| ApiError::internal(format!("read /proc/meminfo: {e}")))?;
    let (cpu_total, cpu_idle) = super::host::cpu_counters(&stat).unwrap_or((0, 0));
    let host = json!({
        "cpu_total": cpu_total,
        "cpu_idle": cpu_idle,
        "cpus": std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        "mem_total_kib": super::host::meminfo_kib(&meminfo, "MemTotal").unwrap_or(0),
        "mem_available_kib": super::host::meminfo_kib(&meminfo, "MemAvailable").unwrap_or(0),
    });
    let (domains, pools) = with_conn(|c| Ok((domains(c)?, pools(c)?)))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "sampled_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "host": host,
        "pools": pools,
        "domains": domains,
    })))
}

fn pools(c: &Connect) -> Result<Vec<Value>, crate::libvirt::LvError> {
    let mut out = Vec::new();
    for p in c.list_all_storage_pools(0)? {
        let active = p.is_active().unwrap_or(false);
        // An inactive pool reports zeros, which would read as an empty disk.
        let info = if active { p.get_info().ok() } else { None };
        out.push(json!({
            "uuid": p.get_uuid_string()?,
            "name": p.get_name().unwrap_or_default(),
            "capacity": info.as_ref().map(|i| i.capacity),
            "allocation": info.as_ref().map(|i| i.allocation),
        }));
    }
    Ok(out)
}

fn domains(c: &Connect) -> Result<Vec<Value>, crate::libvirt::LvError> {
    let mut out = Vec::new();
    for d in crate::libvirt::all_domains(c, 0) {
        out.push(domain(&d)?);
    }
    Ok(out)
}

fn domain(d: &Domain) -> Result<Value, crate::libvirt::LvError> {
    let info = d.get_info().ok();
    let running = d.is_active().unwrap_or(false);
    let (mut rss_kib, mut usable_kib) = (None::<u64>, None::<u64>);
    if running && let Ok(stats) = d.memory_stats(0) {
        for s in stats {
            match s.tag {
                7 => rss_kib = Some(s.val),    // VIR_DOMAIN_MEMORY_STAT_RSS
                8 => usable_kib = Some(s.val), // VIR_DOMAIN_MEMORY_STAT_USABLE
                _ => {}
            }
        }
    }
    // Live XML: a running guest's interfaces carry the `target dev` that
    // interface_stats needs. A failed read fails the whole answer rather than
    // listing the guest with no devices, which would delete their history.
    let xml = d.get_xml_desc(0)?;
    let uuid = d.get_uuid_string()?;
    let nics: Vec<Value> = blocks(&xml, "<interface", "</interface>")
        .into_iter()
        .filter_map(|b| {
            let (dev, mac, _) = super::domains::interface_info(b);
            let counters = dev.as_deref().and_then(|t| d.interface_stats(t).ok());
            Some(json!({
                "mac": mac?,
                "rx_bytes": counters.as_ref().map(|s| s.rx_bytes.max(0)),
                "tx_bytes": counters.as_ref().map(|s| s.tx_bytes.max(0)),
            }))
        })
        .collect();
    let disks: Vec<Value> = disk_targets(&xml)
        .into_iter()
        .map(|dev| {
            let bi = d.get_block_info(&dev, 0).ok();
            json!({
                "dev": dev,
                "capacity": bi.as_ref().map(|b| b.capacity),
                "allocation": bi.as_ref().map(|b| b.allocation),
            })
        })
        .collect();
    Ok(json!({
        "uuid": uuid,
        // `ct` for a container: the web server keys history by UUID either
        // way, but lists and permissions tell the two apart.
        "kind": if xml.contains("<domain type='lxc'") { "ct" } else { "vm" },
        "name": d.get_name().unwrap_or_default(),
        "state": info.as_ref().map(|i| domain_state_name(i.state)).unwrap_or("unknown"),
        "cpu_time_ns": info.as_ref().map(|i| i.cpu_time).unwrap_or(0),
        "vcpus": info.as_ref().map(|i| i.nr_virt_cpu).unwrap_or(0),
        "memory_kib": info.as_ref().map(|i| i.memory).unwrap_or(0),
        "max_memory_kib": info.as_ref().map(|i| i.max_mem).unwrap_or(0),
        "rss_kib": rss_kib,
        "usable_kib": usable_kib,
        "nics": nics,
        "disks": disks,
    }))
}

/// Every `open … close` element in `xml`, as slices.
fn blocks<'a>(xml: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(open) {
        let block = &rest[i..];
        let Some(end) = block.find(close) else { break };
        out.push(&block[..end]);
        rest = &block[end + close.len()..];
    }
    out
}

/// Target devs of the guest's disks (not cdroms or floppies: they hold shared
/// media, whose fill level says nothing about the guest).
fn disk_targets(xml: &str) -> Vec<String> {
    blocks(xml, "<disk", "</disk>")
        .into_iter()
        .filter_map(|b| {
            let open = &b[..b.find('>').unwrap_or(b.len())];
            if attr(open, "device").unwrap_or("disk") != "disk" {
                return None;
            }
            let t = &b[b.find("<target")?..];
            attr(&t[..t.find('>')?], "dev").map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_targets_skip_shared_media() {
        let xml = r#"<devices>
          <disk type='file' device='disk'><source file='/a.qcow2'/><target dev='vda' bus='virtio'/></disk>
          <disk type="volume" device="cdrom"><source pool="ISOs" volume="x.iso"/><target dev="sda"/></disk>
          <disk type='volume'><source pool='p' volume='v'/><target dev='vdb'/></disk>
        </devices>"#;
        assert_eq!(disk_targets(xml), vec!["vda", "vdb"]);
    }

    #[test]
    fn blocks_are_split_in_order() {
        let xml = "<interface a/>x</interface><interface b/>y</interface>";
        let b = blocks(xml, "<interface", "</interface>");
        assert_eq!(b.len(), 2);
        assert!(b[0].contains(" a/") && b[1].contains(" b/"));
    }
}
