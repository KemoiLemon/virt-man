//! `/containers`: LXC guests, run by `crate::lxc` without libvirt.
//!
//! The wire shapes are the ones the SPA and the web server read when
//! containers were libvirt LXC domains, so neither had to learn a new API:
//! the list and summary carry `privileged` and `rootfs`, stats and interfaces
//! are shaped like a VM's, and the definition is still `<domain type='lxc'>`.
//!
//! The identity is the container's UUID, which is `<name>`, `<uuid>` and the
//! URL; the user's display name is `<title>`. Routes accept either.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::firewall::FirewallState;
use super::nics::{check_rate, kib_to_mbps, mbps_to_kib, normalize_mac};
use super::{ApiError, off_runtime};
use crate::ct;
use crate::ctxml::{self, Container, Nic};
use crate::domainxml::{Bandwidth, Rate};
use crate::ipam::server::Ipam;
use crate::lxc::{Entry, Manager, State as CtState};
use crate::pools::Pools;

/// How long a delete, or a Shutdown click, waits for init to power off.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct CtApi {
    cts: Manager,
    pools: Pools,
    ipam: Ipam,
    fw: FirewallState,
    /// Only to refresh the IPAM snapshot, which reads VMs and containers
    /// together, after a NIC change.
    qemu: crate::qemu::Manager,
}

pub fn routes(
    cts: Manager,
    pools: Pools,
    ipam: Ipam,
    fw: FirewallState,
    qemu: crate::qemu::Manager,
) -> Router {
    Router::new()
        .route("/containers", get(list).post(create))
        .route("/containers/{name}", get(get_one).delete(remove))
        .route("/containers/{name}/xml", get(get_xml).put(put_xml))
        .route("/containers/{name}/state", get(get_state))
        .route("/containers/{name}/stats", get(get_stats))
        .route(
            "/containers/{name}/interfaces",
            get(get_interfaces).post(add_nic),
        )
        .route("/containers/{name}/interfaces/{mac}", delete(remove_nic))
        .route(
            "/containers/{name}/interfaces/{mac}/bandwidth",
            put(set_bandwidth),
        )
        .route(
            "/containers/{name}/hardware",
            get(get_hardware).put(put_hardware),
        )
        .route("/containers/{name}/start", post(start))
        .route("/containers/{name}/shutdown", post(shutdown))
        .route("/containers/{name}/reboot", post(reboot))
        .route("/containers/{name}/destroy", post(destroy))
        .route("/containers/{name}/rename", post(rename))
        .route("/containers/{name}/autostart", post(autostart))
        .with_state(CtApi {
            cts,
            pools,
            ipam,
            fw,
            qemu,
        })
}

fn resolve(cts: &Manager, key: &str) -> Result<Entry, ApiError> {
    cts.resolve(key)
        .ok_or_else(|| ApiError::not_found(format!("no container '{key}'")))
}

fn summary(cts: &Manager, e: &Entry) -> Value {
    let state = cts.state(e.id());
    json!({
        "name": e.id(),
        "uuid": e.id(),
        "title": (!e.def.title.is_empty()).then(|| e.def.title.clone()),
        "state": state.as_str(),
        "active": state == CtState::Running,
        "autostart": e.autostart,
        "vcpus": e.def.vcpus,
        "memory_mb": e.def.memory_mib,
        "privileged": e.def.privileged(),
        "rootfs": e.def.rootfs,
    })
}

async fn list(State(s): State<CtApi>) -> Result<Json<Value>, ApiError> {
    let all = s.cts.list().map_err(ApiError::internal)?;
    Ok(Json(json!(
        all.iter().map(|e| summary(&s.cts, e)).collect::<Vec<_>>()
    )))
}

async fn get_one(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    Ok(Json(summary(&s.cts, &e)))
}

async fn get_state(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    Ok(Json(
        json!({"name": e.id(), "state": s.cts.state(e.id()).as_str(), "reason": 0}),
    ))
}

async fn get_stats(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let stats = s.cts.stats(&e);
    let (cpus, memory_kib, mhz, model) = super::host::node_info();
    Ok(Json(json!({
        "name": e.id(),
        "state": stats.state,
        "cpu_time_ns": stats.cpu_time_ns,
        "vcpus": stats.vcpus,
        "memory_kib": stats.memory_kib,
        "max_memory_kib": stats.max_memory_kib,
        "rss_kib": stats.rss_kib,
        "usable_kib": stats.usable_kib,
        "host": {"cpus": cpus, "memory_kib": memory_kib, "mhz": mhz, "model": model},
        "sampled_at_ms": now_ms(),
    })))
}

async fn get_interfaces(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let out: Vec<Value> = s
        .cts
        .interfaces(&e)
        .into_iter()
        .map(|i| {
            json!({
                "dev": i.dev,
                "mac": i.mac,
                "model": i.model,
                "network": i.network.as_deref().and_then(|b| s.ipam.network_for_bridge(b)),
                "rx_mbps": i.rx_kib.map(kib_to_mbps),
                "tx_mbps": i.tx_kib.map(kib_to_mbps),
                "rx_bytes": i.rx_bytes,
                "tx_bytes": i.tx_bytes,
            })
        })
        .collect();
    Ok(Json(json!({"name": e.id(), "interfaces": out})))
}

async fn get_xml(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    Ok(Json(json!({"name": e.id(), "xml": ctxml::render(&e.def)})))
}

/// MACs other guests hold, so a stored container never shares one: it would
/// share a DHCP reservation, and firewall rules scoped to it.
fn macs_elsewhere(ipam: &Ipam, id: &str) -> HashSet<String> {
    ipam.guests
        .lock()
        .expect("guests poisoned")
        .iter()
        .filter(|(_, g)| g.uuid != id)
        .map(|(m, _)| m.clone())
        .collect()
}

/// After a NIC or definition change: reservations first, then the rules keyed
/// by the MACs they now hold.
async fn settle_network(s: &CtApi) -> Option<String> {
    if let Err(e) = crate::ipam::server::supervise_once(&s.ipam, &s.qemu).await {
        tracing::warn!("containers: refreshing IPAM: {e}");
    }
    super::nics::reapply_firewall(&s.fw).await
}

/// The editor's definition. It takes effect at the next start, as a VM's
/// does; what only the daemon decides (the rootfs and its mount, a uid range
/// nobody else holds) is checked by the manager whoever asks.
async fn put_xml(
    State(s): State<CtApi>,
    Path(name): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let xml = super::domains::pin_name(&body, e.id()).map_err(ApiError::bad_request)?;
    let def = ctxml::parse(&xml).map_err(ApiError::bad_request)?;
    let taken = macs_elsewhere(&s.ipam, e.id());
    s.cts
        .store(def, Some(e.id()), &taken)
        .map_err(ApiError::bad_request)?;
    let warning = settle_network(&s).await;
    Ok(Json(super::nics::with_warning(
        json!({"name": e.id(), "defined": true}),
        warning,
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateReq {
    title: String,
    #[serde(default)]
    hostname: Option<String>,
    /// A template volume: `{pool, volume}`.
    template: VolRef,
    /// Where the rootfs image goes, and how big.
    rootfs: RootfsReq,
    memory_mb: u64,
    #[serde(default)]
    swap_mb: u64,
    vcpus: u32,
    /// Network keys (a network's UUID or its title); the container gets a NIC
    /// on each network's bridge.
    #[serde(default)]
    networks: Vec<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    ssh_keys: Option<String>,
    #[serde(default)]
    privileged: bool,
    #[serde(default)]
    start: bool,
    #[serde(default)]
    autostart: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolRef {
    pool: String,
    volume: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootfsReq {
    pool: String,
    size_gb: u64,
}

/// Id ranges handed to creates still unpacking their template. A build takes
/// seconds, and two creates must not both be given the lowest free range.
fn reserved_bases() -> &'static Mutex<HashSet<u32>> {
    static R: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

struct Reservation(Option<u32>);

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(b) = self.0 {
            reserved_bases().lock().expect("bases").remove(&b);
        }
    }
}

fn reserve_base(cts: &Manager) -> Result<Reservation, String> {
    let mut r = reserved_bases().lock().expect("bases");
    let used = cts.used_bases(None)?;
    let b = ct::free_base(used.into_iter().chain(r.iter().copied()));
    r.insert(b);
    Ok(Reservation(Some(b)))
}

/// The rootfs volume's name. The web server records a container's storage by
/// it, so it is fixed: `ct-<uuid>.raw` in the chosen pool.
pub fn rootfs_volume(id: &str) -> String {
    format!("ct-{id}.raw")
}

async fn create(
    State(s): State<CtApi>,
    Json(req): Json<CreateReq>,
) -> Result<Json<Value>, ApiError> {
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("give the container a name"));
    }
    if s.cts.resolve(&title).is_some() {
        return Err(ApiError::conflict(format!(
            "a container named '{title}' already exists"
        )));
    }
    let hostname = req
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| ct::hostname_from(&title));
    ct::check_hostname(&hostname).map_err(ApiError::bad_request)?;
    if !(1..=4096).contains(&req.rootfs.size_gb) {
        return Err(ApiError::bad_request("rootfs size must be 1-4096 GB"));
    }
    if !(16..=1 << 20).contains(&req.memory_mb) || !(1..=256).contains(&req.vcpus) {
        return Err(ApiError::bad_request(
            "memory must be 16 MB-1 TB and vCPUs 1-256",
        ));
    }
    if req.swap_mb > 1 << 20 {
        return Err(ApiError::bad_request("swap must be under 1 TB"));
    }
    if !super::templates::is_template(&req.template.volume) {
        return Err(ApiError::bad_request(format!(
            "'{}' is not a container template",
            req.template.volume
        )));
    }
    let mut nics = Vec::with_capacity(req.networks.len());
    for n in &req.networks {
        let bridge = s
            .ipam
            .bridge_for(n)
            .ok_or_else(|| ApiError::bad_request(format!("unknown network '{n}'")))?;
        nics.push(Nic {
            mac: None,
            bridge,
            bandwidth: Bandwidth::default(),
        });
    }
    let missing = ct::missing_tools();
    if !missing.is_empty() {
        return Err(ApiError::internal(format!(
            "this host is missing {} needed to build a container (install the lxc package)",
            missing.join(", ")
        )));
    }
    let root_hash = match req.password.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(ct::hash_password(p).map_err(ApiError::internal)?),
        None => None,
    };
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    let reservation = if req.privileged {
        Reservation(None)
    } else {
        reserve_base(&s.cts).map_err(ApiError::internal)?
    };
    let id_base = reservation.0;

    let template = s
        .pools
        .volume_path(&req.template.pool, &req.template.volume)
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "no template '{}' in pool {}",
                req.template.volume, req.template.pool
            ))
        })?;
    let image = s
        .pools
        .create_raw(
            &req.rootfs.pool,
            &rootfs_volume(&id),
            req.rootfs.size_gb << 30,
        )
        .map_err(|e| ApiError::bad_request(format!("rootfs in pool {}: {e}", req.rootfs.pool)))?;
    let image = image.to_string_lossy().into_owned();

    let def = Container {
        id: id.clone(),
        title,
        description: String::new(),
        memory_mib: req.memory_mb as u32,
        swap_mib: req.swap_mb as u32,
        vcpus: req.vcpus,
        cpu_limit_pct: 0,
        init: "/sbin/init".into(),
        id_base,
        rootfs: image.clone(),
        nics,
    };
    let (cts, u, img, keys) = (
        s.cts.clone(),
        id.clone(),
        image.clone(),
        req.ssh_keys.clone().unwrap_or_default(),
    );
    let taken = macs_elsewhere(&s.ipam, &id);
    let template = template.to_string_lossy().into_owned();
    let made = tokio::task::spawn_blocking(move || -> Result<Container, String> {
        ct::build(
            &u,
            &img,
            &template,
            id_base,
            &ct::Setup {
                hostname: &hostname,
                root_hash: root_hash.as_deref(),
                ssh_keys: keys.trim(),
            },
        )?;
        cts.store(def, None, &taken)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    drop(reservation);
    let def = match made {
        Ok(d) => d,
        Err(e) => {
            // Never leave a half-made rootfs behind to count against a quota.
            let u = id.clone();
            let _ = tokio::task::spawn_blocking(move || ct::unmount(&u)).await;
            if let Some(p) = s.pools.owned_file(&image) {
                let _ = std::fs::remove_file(p);
            }
            let _ = s.cts.remove(&id);
            return Err(ApiError::bad_request(e));
        }
    };
    if req.autostart {
        s.cts.set_autostart(&id, true).map_err(ApiError::internal)?;
    }
    let warning = settle_network(&s).await;
    // The container exists from here on; a start that fails is reported
    // alongside it, not as a failed create that leaves it behind unexplained.
    let mut started = false;
    let mut start_error = None;
    if req.start {
        let (cts, u) = (s.cts.clone(), id.clone());
        match off_runtime(move || cts.start(&u)).await {
            Ok(()) => started = true,
            Err(e) => start_error = Some(e.message().to_string()),
        }
    }
    let mut out = json!({
        "name": id, "uuid": id, "started": started, "privileged": def.privileged(),
    });
    if let Some(e) = start_error {
        out["start_error"] = json!(e);
    }
    Ok(Json(super::nics::with_warning(out, warning)))
}

/// Delete a container, and its rootfs with it (the rootfs *is* the container;
/// there is no "keep the disk" for one). Templates are never touched. A
/// running container is stopped first, the way a running VM is: asked to
/// power off, then forced.
async fn remove(State(s): State<CtApi>, Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let id = e.id().to_string();
    let stopped = if s.cts.state(&id) == CtState::Running {
        let (cts, u) = (s.cts.clone(), id.clone());
        off_runtime(move || {
            cts.shutdown(&u, SHUTDOWN_WAIT)
                .or_else(|_| cts.force_stop(&u))
        })
        .await?;
        true
    } else {
        false
    };
    s.cts.remove(&id).map_err(ApiError::internal)?;
    let u = id.clone();
    let _ = tokio::task::spawn_blocking(move || ct::unmount(&u)).await;
    let (mut removed, mut failed) = (vec![], vec![]);
    let outcome = s
        .pools
        .owned_file(&e.def.rootfs)
        .ok_or_else(|| "the rootfs is not a file in a storage pool".to_string())
        .and_then(|p| std::fs::remove_file(p).map_err(|e| e.to_string()));
    match outcome {
        Ok(()) => removed.push(e.def.rootfs.clone()),
        Err(err) => failed.push(json!({"disk": e.def.rootfs, "error": err})),
    }
    let warning = settle_network(&s).await;
    Ok(Json(super::nics::with_warning(
        json!({
            "name": id, "undefined": true, "stopped": stopped, "storage_removed": true,
            "removed": removed, "kept": [], "failed": failed,
        }),
        warning,
    )))
}

async fn start(State(s): State<CtApi>, Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let (cts, id) = (s.cts.clone(), e.id().to_string());
    off_runtime(move || cts.start(&id)).await?;
    Ok(Json(json!({"name": e.id(), "started": true})))
}

async fn shutdown(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let (cts, id) = (s.cts.clone(), e.id().to_string());
    off_runtime(move || cts.shutdown(&id, SHUTDOWN_WAIT)).await?;
    Ok(Json(json!({"name": e.id(), "shutdown": true})))
}

async fn reboot(State(s): State<CtApi>, Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let (cts, id) = (s.cts.clone(), e.id().to_string());
    off_runtime(move || cts.reboot(&id)).await?;
    Ok(Json(json!({"name": e.id(), "rebooted": true})))
}

async fn destroy(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let (cts, id) = (s.cts.clone(), e.id().to_string());
    off_runtime(move || cts.force_stop(&id)).await?;
    Ok(Json(json!({"name": e.id(), "destroyed": true})))
}

#[derive(Deserialize)]
struct RenameReq {
    name: String,
}

/// Rename the display name. The identity -- the UUID, the URL, the MACs, the
/// rootfs and its mount -- never changes, so a rename cannot orphan anything,
/// and a running container may be renamed: nothing it runs reads the title.
async fn rename(
    State(s): State<CtApi>,
    Path(name): Path<String>,
    Json(req): Json<RenameReq>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let title = req.name.trim().to_string();
    if title.is_empty() || title.contains(['/', '\n', '\r']) {
        return Err(ApiError::bad_request("give the container a name"));
    }
    if title == e.def.title {
        return Ok(Json(json!({"name": e.id(), "renamed": false})));
    }
    let def = Container {
        title,
        ..e.def.clone()
    };
    s.cts
        .store(def, Some(e.id()), &macs_elsewhere(&s.ipam, e.id()))
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({"name": e.id(), "renamed": true})))
}

#[derive(Deserialize)]
struct AutostartReq {
    autostart: bool,
}

async fn autostart(
    State(s): State<CtApi>,
    Path(name): Path<String>,
    Json(req): Json<AutostartReq>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    s.cts
        .set_autostart(e.id(), req.autostart)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"name": e.id(), "autostart": req.autostart})))
}

#[derive(Deserialize)]
struct AddNicReq {
    /// A network key (its UUID or title); the NIC joins that network's bridge.
    network: String,
    #[serde(default)]
    mac: Option<String>,
    /// Accepted because the VM form sends one; a veth has no model.
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
}

/// Add a NIC. LXC builds a container's veths when it starts, so a running
/// container gets it at its next start and the answer says so (`pending`)
/// rather than claiming an interface the container cannot see yet.
async fn add_nic(
    State(s): State<CtApi>,
    Path(name): Path<String>,
    Json(req): Json<AddNicReq>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let network = req.network.trim();
    let bridge = s
        .ipam
        .bridge_for(network)
        .ok_or_else(|| ApiError::bad_request(format!("unknown network '{network}'")))?;
    let mac = match req.mac.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => Some(normalize_mac(m).map_err(ApiError::bad_request)?),
        None => None,
    };
    let mut def = e.def.clone();
    def.nics.push(Nic {
        mac,
        bridge,
        bandwidth: Bandwidth::default(),
    });
    let stored = s
        .cts
        .store(def, Some(e.id()), &macs_elsewhere(&s.ipam, e.id()))
        .map_err(ApiError::bad_request)?;
    let mac = stored.nics.last().and_then(|n| n.mac.clone());
    let pending = s.cts.state(e.id()) == CtState::Running;
    let warning = settle_network(&s).await;
    Ok(Json(super::nics::with_warning(
        json!({"name": e.id(), "mac": mac, "live": false, "pending": pending}),
        warning,
    )))
}

/// Remove a NIC from the definition. A running container keeps its veth
/// until it stops (`pending`).
async fn remove_nic(
    State(s): State<CtApi>,
    Path((name, mac)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let mut def = e.def.clone();
    let before = def.nics.len();
    def.nics.retain(|n| n.mac.as_deref() != Some(&mac));
    if def.nics.len() == before {
        return Err(ApiError::not_found(format!("no interface with MAC {mac}")));
    }
    s.cts
        .store(def, Some(e.id()), &macs_elsewhere(&s.ipam, e.id()))
        .map_err(ApiError::bad_request)?;
    let pending = s.cts.state(e.id()) == CtState::Running;
    let warning = settle_network(&s).await;
    Ok(Json(super::nics::with_warning(
        json!({"name": e.id(), "mac": mac, "removed": true, "pending": pending}),
        warning,
    )))
}

#[derive(Deserialize)]
struct BandwidthReq {
    #[serde(default)]
    rx_mbps: Option<f64>,
    #[serde(default)]
    tx_mbps: Option<f64>,
}

/// A NIC's rate limits, in the definition and, while the container runs, on
/// the host side of its veth at once.
async fn set_bandwidth(
    State(s): State<CtApi>,
    Path((name, mac)): Path<(String, String)>,
    Json(req): Json<BandwidthReq>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let rx = check_rate(req.rx_mbps, "RX limit").map_err(ApiError::bad_request)?;
    let tx = check_rate(req.tx_mbps, "TX limit").map_err(ApiError::bad_request)?;
    let (rx_kib, tx_kib) = (rx.map(mbps_to_kib), tx.map(mbps_to_kib));
    let mut def = e.def.clone();
    let nic = def
        .nics
        .iter_mut()
        .find(|n| n.mac.as_deref() == Some(&mac))
        .ok_or_else(|| ApiError::not_found(format!("no interface with MAC {mac}")))?;
    let rate = |kib: Option<u64>| {
        kib.map(|k| Rate {
            average: Some(k),
            ..Default::default()
        })
    };
    nic.bandwidth = Bandwidth {
        inbound: rate(rx_kib),
        outbound: rate(tx_kib),
    };
    s.cts
        .store(def, Some(e.id()), &macs_elsewhere(&s.ipam, e.id()))
        .map_err(ApiError::bad_request)?;
    let live = s
        .cts
        .apply_bandwidth(e.id(), &mac, rx_kib, tx_kib)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "name": e.id(), "mac": mac, "live": live, "pending": !live,
        "rx_mbps": rx.map(|x| kib_to_mbps(mbps_to_kib(x))),
        "tx_mbps": tx.map(|x| kib_to_mbps(mbps_to_kib(x))),
    })))
}

/// A container's resources, as its hardware editor shows them.
#[derive(serde::Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Hardware {
    pub vcpus: u32,
    pub memory_mb: u64,
    /// 0 = no swap beyond memory.
    #[serde(default)]
    pub swap_mb: u64,
    /// CPU cap in whole-CPU percent (150 = one and a half CPUs); 0 = none.
    #[serde(default)]
    pub cpu_limit_pct: u32,
    /// The rootfs size. It can only grow: shrinking ext4 under a running
    /// container is not safe, and there is no undo if it goes wrong.
    pub disk_gb: u64,
}

fn hardware_of(c: &Container) -> Hardware {
    Hardware {
        vcpus: c.vcpus,
        memory_mb: u64::from(c.memory_mib),
        swap_mb: u64::from(c.swap_mib),
        cpu_limit_pct: c.cpu_limit_pct,
        // A raw rootfs image: its length is the disk the container sees.
        disk_gb: std::fs::metadata(&c.rootfs)
            .map(|m| m.len().div_ceil(1 << 30))
            .unwrap_or(0),
    }
}

fn check_hardware(hw: &Hardware) -> Result<(), String> {
    if !(16..=1 << 20).contains(&hw.memory_mb) {
        return Err("memory must be 16 MB-1 TB".into());
    }
    if !(1..=256).contains(&hw.vcpus) {
        return Err("vCPUs must be 1-256".into());
    }
    if hw.swap_mb > 1 << 20 {
        return Err("swap must be under 1 TB".into());
    }
    if hw.cpu_limit_pct > hw.vcpus * 100 {
        return Err(format!(
            "a CPU limit of {}% is more than {} vCPU(s) can use; leave it 0 for none",
            hw.cpu_limit_pct, hw.vcpus
        ));
    }
    if !(1..=4096).contains(&hw.disk_gb) {
        return Err("the root disk must be 1-4096 GB".into());
    }
    Ok(())
}

/// `c` with `hw`'s sizes in place of its own.
fn with_hardware(c: &Container, hw: &Hardware) -> Container {
    Container {
        vcpus: hw.vcpus,
        memory_mib: hw.memory_mb as u32,
        swap_mib: hw.swap_mb as u32,
        cpu_limit_pct: hw.cpu_limit_pct,
        ..c.clone()
    }
}

async fn get_hardware(
    State(s): State<CtApi>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let e = resolve(&s.cts, &name)?;
    Ok(Json(json!(hardware_of(&e.def))))
}

/// Change a container's resources. Memory, swap, the CPU cap and the vCPU set
/// are cgroup v2 files, so a running container gets every one of them at once
/// (`applied`); nothing waits for a restart. A bigger root disk is grown now,
/// running or not: the volume, its loop device, then ext4 online.
async fn put_hardware(
    State(s): State<CtApi>,
    Path(name): Path<String>,
    Json(hw): Json<Hardware>,
) -> Result<Json<Value>, ApiError> {
    check_hardware(&hw).map_err(ApiError::bad_request)?;
    let e = resolve(&s.cts, &name)?;
    let cur = hardware_of(&e.def);
    if hw.disk_gb < cur.disk_gb {
        return Err(ApiError::bad_request(format!(
            "the root disk can only grow ({} GB now); shrinking it could lose data",
            cur.disk_gb
        )));
    }
    let next = with_hardware(&e.def, &hw);
    s.cts
        .store(next.clone(), Some(e.id()), &macs_elsewhere(&s.ipam, e.id()))
        .map_err(ApiError::bad_request)?;
    let running = s.cts.state(e.id()) == CtState::Running;
    let mut applied = vec![];
    if running {
        s.cts.apply_limits(&next).map_err(|err| {
            ApiError::internal(format!(
                "saved, but not applied to the running container: {err}"
            ))
        })?;
        for (changed, what) in [
            (hw.memory_mb != cur.memory_mb, "memory"),
            (hw.swap_mb != cur.swap_mb, "swap"),
            (hw.cpu_limit_pct != cur.cpu_limit_pct, "cpu limit"),
            (hw.vcpus != cur.vcpus, "vCPUs"),
        ] {
            if changed {
                applied.push(what);
            }
        }
    }
    let mut grown = false;
    if hw.disk_gb > cur.disk_gb {
        // The rootfs is a raw file in a pool: grow the file, then the ext4
        // inside it. Never touch a path outside a pool.
        let path = s
            .pools
            .owned_file(&e.def.rootfs)
            .ok_or_else(|| ApiError::bad_request("the rootfs is not a file in a storage pool"))?;
        let (image, size) = (e.def.rootfs.clone(), hw.disk_gb << 30);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            file.set_len(size)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            ct::grow_fs(&image)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
        grown = true;
    }
    Ok(Json(json!({
        "name": e.id(), "running": running, "applied": applied, "pending": [],
        "disk_grown": grown, "hardware": hw,
    })))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const ID: &str = "0b776cc0-f6b1-4bbd-b67b-88ccc6c24a76";

    struct Fixture {
        app: Router,
        cts: Manager,
        dir: std::path::PathBuf,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("ctapi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cts = Manager::with_paths(dir.join("containers.json"), dir.join("lxc"));
        let ipam = Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let pools = Pools::load(dir.join("pools.json")).unwrap();
        let qemu =
            crate::qemu::Manager::with_qemu(dir.join("data"), dir.join("state"), "/none").unwrap();
        let fw = FirewallState {
            path: dir.join("firewall.json"),
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            ipam: ipam.clone(),
        };
        cts.store(
            Container {
                id: ID.into(),
                title: "web".into(),
                description: String::new(),
                memory_mib: 512,
                swap_mib: 0,
                vcpus: 1,
                cpu_limit_pct: 0,
                init: "/sbin/init".into(),
                id_base: Some(ct::ID_BASE),
                rootfs: dir.join("ct.raw").to_string_lossy().into_owned(),
                nics: vec![],
            },
            None,
            &HashSet::new(),
        )
        .unwrap();
        std::fs::write(dir.join("ct.raw"), b"").unwrap();
        let app = routes(cts.clone(), pools, ipam, fw, qemu);
        Fixture { app, cts, dir }
    }

    async fn call(app: &Router, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status().as_u16();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    #[tokio::test]
    async fn a_container_reads_by_uuid_or_title_in_the_old_shapes() {
        let f = fixture("read");
        let (st, list) = call(&f.app, "GET", "/containers", "").await;
        assert_eq!(st, 200);
        assert_eq!(list[0]["name"], ID);
        assert_eq!(list[0]["title"], "web");
        assert_eq!(list[0]["state"], "stopped");
        assert_eq!(list[0]["privileged"], false);
        let (_, by_title) = call(&f.app, "GET", "/containers/web", "").await;
        let (_, by_id) = call(&f.app, "GET", &format!("/containers/{ID}"), "").await;
        assert_eq!(by_title, by_id);
        let (st, _) = call(&f.app, "GET", "/containers/nope", "").await;
        assert_eq!(st, 404);
        let (_, xml) = call(&f.app, "GET", "/containers/web/xml", "").await;
        assert!(xml["xml"].as_str().unwrap().contains("type='lxc'"));
        let (_, hw) = call(&f.app, "GET", "/containers/web/hardware", "").await;
        assert_eq!(
            (hw["memory_mb"].as_u64(), hw["vcpus"].as_u64()),
            (Some(512), Some(1))
        );
        let _ = std::fs::remove_dir_all(f.dir);
    }

    #[tokio::test]
    async fn edits_keep_the_rootfs_and_apply_to_the_definition() {
        let f = fixture("edit");
        let (st, _) = call(
            &f.app,
            "PUT",
            "/containers/web/hardware",
            r#"{"vcpus":2,"memory_mb":1024,"swap_mb":128,"cpu_limit_pct":150,"disk_gb":0}"#,
        )
        .await;
        assert_eq!(st, 400, "a 0 GB disk is refused");
        let (st, v) = call(&f.app, "POST", "/containers/web/rename", r#"{"name":"db"}"#).await;
        assert_eq!((st, v["renamed"].as_bool()), (200, Some(true)));
        assert_eq!(f.cts.get(ID).unwrap().def.title, "db");
        let (st, _) = call(
            &f.app,
            "POST",
            "/containers/db/autostart",
            r#"{"autostart":true}"#,
        )
        .await;
        assert_eq!(st, 200);
        assert!(f.cts.get(ID).unwrap().autostart);

        // The XML tab: a moved rootfs is refused, a memory change is stored,
        // and a display name in <name> is put back to the id.
        let (_, xml) = call(&f.app, "GET", "/containers/db/xml", "").await;
        let xml = xml["xml"].as_str().unwrap().to_string();
        let moved = xml.replace("ct.raw", "other.raw");
        let (st, _) = call(&f.app, "PUT", "/containers/db/xml", &moved).await;
        assert_eq!(st, 400);
        let bigger = xml
            .replace("<memory unit='MiB'>512<", "<memory unit='MiB'>768<")
            .replace(&format!("<name>{ID}</name>"), "<name>db</name>");
        let (st, v) = call(&f.app, "PUT", "/containers/db/xml", &bigger).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(f.cts.get(ID).unwrap().def.memory_mib, 768);
        let (st, _) = call(
            &f.app,
            "POST",
            "/containers/db/interfaces",
            r#"{"network":"nope"}"#,
        )
        .await;
        assert_eq!(st, 400, "an unknown network");
        let _ = std::fs::remove_dir_all(f.dir);
    }

    #[test]
    fn hardware_is_checked() {
        let hw = Hardware {
            vcpus: 2,
            memory_mb: 512,
            swap_mb: 0,
            cpu_limit_pct: 150,
            disk_gb: 8,
        };
        assert!(check_hardware(&hw).is_ok());
        assert!(
            check_hardware(&Hardware {
                cpu_limit_pct: 500,
                ..hw.clone()
            })
            .is_err(),
            "cap above vCPUs"
        );
        assert!(check_hardware(&Hardware { disk_gb: 0, ..hw }).is_err());
    }

    #[test]
    fn reserved_ranges_are_not_handed_out_twice() {
        let dir = std::env::temp_dir().join(format!("ctapi-bases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cts = Manager::with_paths(dir.join("c.json"), dir.join("lxc"));
        let a = reserve_base(&cts).unwrap();
        let b = reserve_base(&cts).unwrap();
        assert_ne!(a.0, b.0);
        let first = a.0;
        drop(a);
        assert_eq!(reserve_base(&cts).unwrap().0, first, "released on drop");
        let _ = std::fs::remove_dir_all(dir);
    }
}
