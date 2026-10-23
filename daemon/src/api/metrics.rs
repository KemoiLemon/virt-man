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
use axum::extract::Extension;
use axum::routing::get;
use serde_json::{Value, json};

use super::ApiError;
use crate::domainxml::DiskDevice;
use crate::pools::Pools;
use crate::qemu::{Manager, VmConfig};

pub fn routes() -> Router {
    Router::new().route("/metrics", get(metrics))
}

async fn metrics(
    Extension(pools): Extension<Pools>,
    Extension(manager): Extension<Manager>,
) -> Result<Json<Value>, ApiError> {
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
    let domains = manager
        .list()
        .map_err(ApiError::internal)?
        .iter()
        .map(|cfg| domain(&manager, cfg))
        .collect::<Result<Vec<_>, String>>()
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "sampled_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "host": host,
        "pools": pool_metrics(&pools),
        "domains": domains,
    })))
}

fn pool_metrics(pools: &Pools) -> Vec<Value> {
    pools
        .list()
        .iter()
        .map(|(key, p)| {
            // An inactive pool reports no figures: zeros would read as an
            // empty disk in the history chart.
            let active = p.active && p.path.is_dir();
            let (capacity, allocation, available) = if active {
                match pools.stats(key) {
                    Some((c, a, v)) => (c, a, v),
                    None => (None, None, None),
                }
            } else {
                (None, None, None)
            };
            json!({
                "uuid": key,
                "name": key,
                "capacity": capacity,
                "allocation": allocation,
                "available": available,
            })
        })
        .collect()
}

/// One guest as the collector reads it. The definition supplies the device
/// list, so a failed counter read is a zero on a known NIC, never a missing
/// device whose history would be deleted.
fn domain(manager: &Manager, cfg: &VmConfig) -> Result<Value, String> {
    use std::os::unix::fs::MetadataExt;
    let d = crate::domainxml::parse(&cfg.xml)?;
    let stats = manager.stats(&cfg.id)?;
    let nics: Vec<Value> = manager
        .interfaces(&cfg.id)?
        .into_iter()
        .filter_map(|i| {
            Some(json!({
                "name": i.dev.unwrap_or_default(),
                "mac": i.mac?,
                "rx_bytes": i.rx_bytes,
                "tx_bytes": i.tx_bytes,
            }))
        })
        .collect();
    let blocks = manager.block_stats(&cfg.id).unwrap_or_default();
    let disks: Vec<Value> = d
        .disks
        .iter()
        .filter(|x| x.device == DiskDevice::Disk)
        .map(|x| {
            // Capacity is the qcow2 virtual size when the header has one, as
            // the storage pool reports it; allocation is the file's blocks.
            let (capacity, allocation) = match &x.source {
                Some(path) => match std::fs::metadata(path) {
                    Ok(meta) => (
                        Some(crate::pools::qcow2_capacity(path).unwrap_or(meta.len())),
                        Some(meta.blocks().saturating_mul(512)),
                    ),
                    Err(_) => (None, None),
                },
                None => (None, None),
            };
            let (read_bytes, write_bytes) = blocks.get(&x.target).copied().unwrap_or((0, 0));
            json!({
                // `dev` is what the history collector keys a disk series by;
                // `target` names the same device in the definition.
                "dev": x.target,
                "target": x.target,
                "capacity": capacity,
                "allocation": allocation,
                "read_bytes": read_bytes,
                "write_bytes": write_bytes,
            })
        })
        .collect();
    Ok(json!({
        "uuid": cfg.id,
        "kind": "vm",
        "name": cfg.name(),
        "state": stats.state,
        "cpu_time_ns": stats.cpu_time_ns,
        "vcpus": u32::from(stats.vcpus),
        "memory_kib": stats.memory_kib,
        "max_memory_kib": stats.max_memory_kib,
        "rss_kib": stats.rss_kib,
        "usable_kib": stats.usable_kib,
        "nics": nics,
        "disks": disks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::Manager;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("metrics-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stopped guest still reports its memory, one zeroed NIC and one disk,
    /// so the collector neither guesses nor loses their series.
    #[test]
    fn a_stopped_guest_reports_its_shape_with_zero_counters() {
        let dir = scratch("stopped");
        let disk = dir.join("disk.raw");
        std::fs::write(&disk, b"hello").unwrap();
        let iso = dir.join("a.iso");
        std::fs::write(&iso, b"iso").unwrap();
        let manager =
            Manager::with_qemu(dir.join("data"), dir.join("state"), "/nonexistent/qemu").unwrap();
        let xml = format!(
            r#"<domain type="kvm">
  <name>{ID}</name>
  <uuid>{ID}</uuid>
  <title>web</title>
  <memory unit="KiB">524288</memory>
  <vcpu>2</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os>
  <devices>
    <disk type="file" device="disk">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <disk type="file" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="sda" bus="sata"/>
    </disk>
    <interface type="bridge">
      <source bridge="vbr0"/>
      <model type="virtio"/>
      <mac address="52:54:00:aa:bb:01"/>
    </interface>
  </devices>
</domain>"#,
            disk.display(),
            iso.display()
        );
        let cfg = manager.define(&xml).unwrap();
        let row = domain(&manager, &cfg).unwrap();
        assert_eq!(row["uuid"], ID);
        assert_eq!(row["name"], "web");
        assert_eq!(row["state"], "stopped");
        assert_eq!(row["cpu_time_ns"], 0);
        assert_eq!(row["vcpus"], 2);
        assert_eq!(row["memory_kib"], 512 * 1024);
        assert_eq!(row["max_memory_kib"], 512 * 1024);
        assert_eq!(row["nics"][0]["mac"], "52:54:00:aa:bb:01");
        assert_eq!(row["nics"][0]["name"], "");
        assert_eq!(row["nics"][0]["rx_bytes"], 0);
        assert_eq!(row["nics"][0]["tx_bytes"], 0);
        assert_eq!(row["disks"].as_array().unwrap().len(), 1, "no cdrom");
        assert_eq!(row["disks"][0]["dev"], "vda");
        assert_eq!(row["disks"][0]["read_bytes"], 0);
        assert_eq!(row["disks"][0]["write_bytes"], 0);
        assert_eq!(row["disks"][0]["capacity"], 5);
        assert!(row["disks"][0]["allocation"].as_u64().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
