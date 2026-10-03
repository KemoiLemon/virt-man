//! `/containers`: LXC guests, served by the same handlers as `/domains`.
//!
//! A container is a libvirt domain on `lxc:///system`. Rather than a second
//! copy of every lifecycle, XML, NIC and firewall handler, `/containers/...`
//! is rewritten to `/domains/...` and run inside `DRIVER.scope(Lxc)`, which
//! makes `with_conn` open the LXC driver. The two trees still never mix: a
//! `/domains` request cannot reach a container nor the reverse, which is what
//! lets the web server grant VMs and containers separately (`VM.*`/`CT.*`).
//!
//! Only what differs is here: creating one from a template, deleting one
//! (its rootfs is not a `<disk>`), starting one (its rootfs must be mounted
//! first), live NIC rate limits (the LXC driver cannot change them live), and
//! the list, which reports `privileged`.

use axum::extract::{Path, Request};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use virt::domain::Domain;
use virt::storage_vol::StorageVol;

use super::ApiError;
use crate::ct;
use crate::libvirt::{DRIVER, Driver, domain_state_name, with_conn};

/// Run `f` against the LXC driver.
pub async fn lxc<F: std::future::Future>(f: F) -> F::Output {
    DRIVER.scope(Driver::Lxc, f).await
}

pub fn routes() -> Router {
    Router::new()
        .route("/containers", get(list).post(create))
        .route("/containers/{name}", get(get_one).delete(delete))
        .route("/containers/{name}/start", post(start))
        .route(
            "/containers/{name}/interfaces/{mac}/bandwidth",
            axum::routing::put(bandwidth),
        )
        .route(
            "/containers/{name}/hardware",
            get(get_hardware).put(put_hardware),
        )
}

/// Everything else under `/containers/{name}/...` is a `/domains` route with
/// the LXC driver. Called with the whole API router, so the rewritten request
/// reaches exactly the handler a VM's would. The console routes are the
/// exception: a container's console is a text stream (`ct_console`).
pub async fn forward(api: Router, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let Some(rest) = path.strip_prefix("/containers/") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Routes a container does not have: graphics, a VNC console, capture of
    // taps is fine (it reads the domain's live XML), reset is not in LXC.
    let sub = rest.split_once('/').map(|x| x.1).unwrap_or("");
    let allowed = matches!(
        sub.split('/').next().unwrap_or(""),
        "xml"
            | "state"
            | "stats"
            | "interfaces"
            | "shutdown"
            | "reboot"
            | "destroy"
            | "rename"
            | "firewall"
            | "capture"
    );
    if !allowed {
        return ApiError::not_found(format!("no such container route: {path}")).into_response();
    }
    let req = if sub == "xml" && req.method() == Method::PUT {
        let id = rest.split('/').next().unwrap_or("").to_string();
        match check_redefine(id, req).await {
            Ok(r) => r,
            Err(e) => return e.into_response(),
        }
    } else {
        req
    };
    let mut new = format!("/domains/{rest}");
    if let Some(q) = req.uri().query() {
        new.push('?');
        new.push_str(q);
    }
    // A fresh request, not the old one with its URI swapped: axum keeps the
    // matched path parameters in the extensions, and the inner router appends
    // its own, so a handler taking `Path<String>` saw three and failed.
    let (parts, body) = req.into_parts();
    let mut b = Request::builder().method(parts.method).uri(new);
    for (k, v) in &parts.headers {
        b = b.header(k, v);
    }
    let Ok(req) = b.body(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    use tower::ServiceExt;
    lxc(async move { api.oneshot(req).await.into_response() }).await
}

/// A redefinition must keep what only the daemon can set up: the rootfs
/// image it records, and the mount the definition names for it. Losing
/// either leaves a container that cannot start (or one whose rootfs the
/// in-use check no longer protects). Everything else -- memory, NICs, the
/// idmap -- is the web server's to allow or refuse.
async fn check_redefine(id: String, req: Request) -> Result<Request, ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .map_err(|e| ApiError::bad_request(format!("body: {e}")))?;
    let new = String::from_utf8_lossy(&bytes).to_string();
    let cur = lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &id)?;
        Ok((
            d.get_uuid_string()?,
            d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?,
        ))
    }))
    .await
    .map_err(ApiError::not_found)?;
    ct::same_rootfs(&cur.1, &new, &cur.0).map_err(ApiError::bad_request)?;
    Ok(Request::from_parts(parts, axum::body::Body::from(bytes)))
}

fn summary(d: &Domain) -> Value {
    let info = d.get_info().ok();
    let xml = d
        .get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)
        .unwrap_or_default();
    json!({
        "name": d.get_name().unwrap_or_default(),
        "uuid": d.get_uuid_string().unwrap_or_default(),
        "title": crate::libvirt::title_of(&xml),
        "state": d.get_state().map(|(s, _)| domain_state_name(s)).unwrap_or("unknown"),
        "active": d.is_active().unwrap_or(false),
        "autostart": d.get_autostart().unwrap_or(false),
        "vcpus": info.as_ref().map(|i| i.nr_virt_cpu).unwrap_or(0),
        "memory_mb": info.as_ref().map(|i| i.max_mem / 1024).unwrap_or(0),
        "privileged": ct::is_privileged(&xml),
        "rootfs": ct::rootfs_of(&xml),
    })
}

/// An unreachable LXC driver is reported as such, so the UI can say how to
/// install it rather than show a raw socket error.
fn driver_error(e: String) -> ApiError {
    if e.contains("libvirt connect") {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the libvirt LXC driver is not available on this host: {e}"),
        )
    } else {
        ApiError::internal(e)
    }
}

async fn list() -> Result<Json<Value>, ApiError> {
    lxc(with_conn(|c| {
        Ok(json!(
            c.list_all_domains(0)?
                .iter()
                .map(summary)
                .collect::<Vec<_>>()
        ))
    }))
    .await
    .map(Json)
    .map_err(driver_error)
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    lxc(with_conn(move |c| {
        Ok(summary(&crate::libvirt::domain(c, &name)?))
    }))
    .await
    .map(Json)
    .map_err(ApiError::not_found)
}

/// Mount its rootfs, then start it. A reboot of the host unmounts every
/// rootfs, and libvirt knows nothing about the image behind the directory.
async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        mount_for(&d)?;
        d.create()?;
        Ok(json!({"name": name, "started": true}))
    }))
    .await
    .map(Json)
    .map_err(ApiError::internal)
}

fn mount_for(d: &Domain) -> Result<(), crate::libvirt::LvError> {
    let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
    let uuid = d.get_uuid_string()?;
    let image =
        ct::rootfs_of(&xml).ok_or_else(|| "this container records no rootfs image".to_string())?;
    let dir = ct::ensure_mounted(&uuid, &image)?;
    // A switch between privileged and unprivileged changed the mapping but
    // not the files; make them agree, or root owns nothing it boots into.
    if !d.is_active().unwrap_or(false) && ct::reconcile_owner(&dir, ct::root_uid(&xml))? {
        tracing::info!("container {uuid}: rootfs re-owned for its uid mapping");
    }
    Ok(())
}

/// Mount every container's rootfs, and start the autostart ones libvirt could
/// not (their rootfs was not mounted at boot). Runs at daemon start.
pub async fn restore() {
    let r = lxc(with_conn(|c| {
        let mut started = 0;
        for d in c.list_all_domains(0)? {
            if let Err(e) = mount_for(&d) {
                tracing::warn!("container {}: {e}", d.get_name().unwrap_or_default());
                continue;
            }
            if d.get_autostart().unwrap_or(false)
                && !d.is_active().unwrap_or(true)
                && d.create().is_ok()
            {
                started += 1;
            }
        }
        Ok(started)
    }))
    .await;
    match r {
        Ok(n) if n > 0 => tracing::info!("started {n} autostart container(s)"),
        Ok(_) => {}
        Err(e) => tracing::info!("containers: {e}"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateReq {
    pub title: String,
    #[serde(default)]
    pub hostname: Option<String>,
    /// A template volume: `{pool, volume}`.
    pub template: VolRef,
    /// Where the rootfs image goes, and how big.
    pub rootfs: RootfsReq,
    pub memory_mb: u64,
    #[serde(default)]
    pub swap_mb: u64,
    pub vcpus: u32,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh_keys: Option<String>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub start: bool,
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolRef {
    pub pool: String,
    pub volume: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsReq {
    pub pool: String,
    pub size_gb: u64,
}

/// A hostname as RFC 1123 allows: what is written into `/etc/hostname`.
pub fn check_hostname(h: &str) -> Result<(), String> {
    let ok = !h.is_empty()
        && h.len() <= 63
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "'{h}' is not a valid hostname (letters, digits and '-', up to 63)"
        ))
    }
}

/// A hostname from a display name: lower-case, other characters to '-'.
pub fn hostname_from(title: &str) -> String {
    let mut h: String = title
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    h.truncate(63);
    let h = h.trim_matches('-').to_string();
    if h.is_empty() { "ct".into() } else { h }
}

fn new_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

async fn create(Json(req): Json<CreateReq>) -> Result<Json<Value>, ApiError> {
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("give the container a name"));
    }
    let hostname = req
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| hostname_from(&title));
    check_hostname(&hostname).map_err(ApiError::bad_request)?;
    if !(1..=4096).contains(&req.rootfs.size_gb) {
        return Err(ApiError::bad_request("rootfs size must be 1-4096 GB"));
    }
    if !(16..=1 << 20).contains(&req.memory_mb) || !(1..=256).contains(&req.vcpus) {
        return Err(ApiError::bad_request(
            "memory must be 16 MB-1 TB and vCPUs 1-256",
        ));
    }
    if !crate::api::templates::is_template(&req.template.volume) {
        return Err(ApiError::bad_request(format!(
            "'{}' is not a container template",
            req.template.volume
        )));
    }
    let missing = ct::missing_tools();
    if !missing.is_empty() {
        return Err(ApiError::internal(format!(
            "this host is missing {} needed to build a container",
            missing.join(", ")
        )));
    }
    let root_hash = match req.password.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(ct::hash_password(p).map_err(ApiError::internal)?),
        None => None,
    };
    let uuid = new_uuid();
    let id_base = if req.privileged {
        None
    } else {
        Some(ct::allocate_ids(&uuid).map_err(ApiError::internal)?)
    };

    // Resolve the template and make the rootfs volume, on the qemu driver
    // (the storage driver is shared, and that is where pools are managed).
    let (tpool, tvol, rpool, size) = (
        req.template.pool.clone(),
        req.template.volume.clone(),
        req.rootfs.pool.clone(),
        req.rootfs.size_gb << 30,
    );
    let vol_name = format!("ct-{uuid}.raw");
    let vn = vol_name.clone();
    let (template_path, image) = with_conn(move |c| {
        let tp = crate::libvirt::pool(c, &tpool)?;
        super::pools::refresh_for_read(&tp);
        let t = StorageVol::lookup_by_name(&tp, &tvol)
            .map_err(|_| format!("no template '{tvol}' in pool {tpool}"))?;
        let rp = crate::libvirt::pool(c, &rpool)?;
        let v = StorageVol::create_xml(&rp, &super::volumes::volume_xml(&vn, size), 0)?;
        Ok((t.get_path()?, v.get_path()?))
    })
    .await
    .map_err(|e| {
        if !req.privileged {
            ct::release_ids(&uuid);
        }
        ApiError::bad_request(e)
    })?;

    let networks = req.networks.clone();
    let (u, img, keys) = (
        uuid.clone(),
        image.clone(),
        req.ssh_keys.clone().unwrap_or_default(),
    );
    let built = tokio::task::spawn_blocking(move || {
        ct::build(
            &u,
            &img,
            &template_path,
            id_base,
            &ct::Setup {
                hostname: &hostname,
                root_hash: root_hash.as_deref(),
                ssh_keys: keys.trim(),
            },
        )
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    let (start, autostart, memory_mb, swap_mb, vcpus) = (
        req.start,
        req.autostart,
        req.memory_mb,
        req.swap_mb,
        req.vcpus,
    );
    let (u, img) = (uuid.clone(), image.clone());
    let defined = match built {
        Err(e) => Err(e),
        Ok(()) => lxc(with_conn(move |c| {
            let xml = ct::domain_xml(&ct::Spec {
                uuid: &u,
                title: &title,
                memory_mb,
                swap_mb,
                vcpus,
                rootfs: &img,
                networks: &networks,
                id_base,
            });
            let d = Domain::define_xml(c, &xml)?;
            if autostart {
                let _ = d.set_autostart(true);
            }
            let started = start && d.create().is_ok();
            Ok(json!({"name": u, "uuid": u, "started": started, "privileged": id_base.is_none()}))
        }))
        .await,
    };
    match defined {
        Ok(v) => Ok(Json(v)),
        Err(e) => {
            // Never leave a half-made rootfs behind to count against a quota.
            let (u, img) = (uuid.clone(), image);
            let _ = tokio::task::spawn_blocking(move || ct::unmount(&u)).await;
            let _ = with_conn(move |c| {
                StorageVol::lookup_by_path(c, &img)?.delete(0)?;
                Ok(())
            })
            .await;
            ct::release_ids(&uuid);
            Err(ApiError::bad_request(e))
        }
    }
}

/// Delete a container, and its rootfs with it (the rootfs is the container;
/// there is no "keep the disk" for one). Templates are never touched.
///
/// A running container is stopped first, the same way a running VM is: the
/// rootfs cannot be unmounted or deleted while its processes hold it, and
/// undefining a live domain would leave it orphaned. So this asks the
/// container to shut down, waits (bounded), then forces it off before the
/// definition and its rootfs go.
async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let stopped = lxc(crate::libvirt::stop_guest(
        &name,
        std::time::Duration::from_secs(30),
    ))
    .await
    .map_err(ApiError::internal)?;
    let (uuid, image, name) = lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        let uuid = d.get_uuid_string()?;
        d.undefine()?;
        Ok((uuid, ct::rootfs_of(&xml), name))
    }))
    .await
    .map_err(ApiError::bad_request)?;
    let u = uuid.clone();
    let _ = tokio::task::spawn_blocking(move || ct::unmount(&u)).await;
    ct::release_ids(&uuid);
    let (mut removed, mut failed) = (vec![], vec![]);
    if let Some(img) = image {
        let i = img.clone();
        match with_conn(move |c| {
            StorageVol::lookup_by_path(c, &i)?.delete(0)?;
            Ok(())
        })
        .await
        {
            Ok(()) => removed.push(img),
            Err(e) => failed.push(json!({"disk": img, "error": e})),
        }
    }
    Ok(Json(json!({
        "name": name, "undefined": true, "stopped": stopped, "storage_removed": true,
        "removed": removed, "kept": [], "failed": failed,
    })))
}

#[derive(Deserialize)]
struct BandwidthReq {
    #[serde(default)]
    rx_mbps: Option<f64>,
    #[serde(default)]
    tx_mbps: Option<f64>,
}

/// The LXC driver cannot change a running container's NIC ("Unable to modify
/// live devices", measured), so a limit is written to the definition and
/// takes effect at the next start. The answer says so (`pending`), and the
/// UI tells the user, rather than claiming a limit that is not enforced.
async fn bandwidth(
    Path((name, mac)): Path<(String, String)>,
    Json(req): Json<BandwidthReq>,
) -> Result<Json<Value>, ApiError> {
    use super::nics::{interface_block, kib_to_mbps, mbps_to_kib, normalize_mac, with_bandwidth};
    let mac = normalize_mac(&mac).map_err(ApiError::bad_request)?;
    let rx = super::nics::check_rate(req.rx_mbps, "RX limit").map_err(ApiError::bad_request)?;
    let tx = super::nics::check_rate(req.tx_mbps, "TX limit").map_err(ApiError::bad_request)?;
    lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        let config = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        let block =
            interface_block(&config, &mac).ok_or_else(|| format!("no interface with MAC {mac}"))?;
        let updated = with_bandwidth(block, rx.map(mbps_to_kib), tx.map(mbps_to_kib));
        d.update_device_flags(&updated, virt::sys::VIR_DOMAIN_AFFECT_CONFIG)?;
        Ok(json!({
            "name": name, "mac": mac, "live": false,
            "pending": d.is_active().unwrap_or(false),
            "rx_mbps": rx.map(|x| kib_to_mbps(mbps_to_kib(x))),
            "tx_mbps": tx.map(|x| kib_to_mbps(mbps_to_kib(x))),
        }))
    }))
    .await
    .map(Json)
    .map_err(ApiError::bad_request)
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

fn mib_of(xml: &str, tag: &str) -> u64 {
    let Some(i) = xml.find(&format!("<{tag}")) else {
        return 0;
    };
    let t = &xml[i..];
    let Some(end) = t.find('>') else { return 0 };
    let unit = crate::disks::attr(&t[..end], "unit").unwrap_or("KiB");
    let v: u64 = t[end + 1..]
        .split('<')
        .next()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    match unit {
        "MiB" | "M" => v,
        "GiB" | "G" => v * 1024,
        "b" | "bytes" => v >> 20,
        _ => v / 1024,
    }
}

fn read_hardware(
    d: &Domain,
    c: &virt::connect::Connect,
) -> Result<(Hardware, String), crate::libvirt::LvError> {
    let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
    let memory_mb = mib_of(&xml, "memory");
    let hard = mib_of(&xml, "swap_hard_limit");
    let vcpus = xml
        .find("<vcpu")
        .and_then(|i| xml[i..].split('>').nth(1))
        .and_then(|t| t.split('<').next()?.trim().parse().ok())
        .unwrap_or(1);
    let quota: i64 = xml
        .find("<quota>")
        .and_then(|i| xml[i + 7..].split('<').next()?.trim().parse().ok())
        .unwrap_or(0);
    let period: i64 = xml
        .find("<period>")
        .and_then(|i| xml[i + 8..].split('<').next()?.trim().parse().ok())
        .unwrap_or(100_000);
    let image =
        ct::rootfs_of(&xml).ok_or_else(|| "this container records no rootfs image".to_string())?;
    let disk_gb = StorageVol::lookup_by_path(c, &image)
        .and_then(|v| v.get_info())
        .map(|i| i.capacity.div_ceil(1 << 30))
        .unwrap_or(0);
    Ok((
        Hardware {
            vcpus,
            memory_mb,
            swap_mb: hard.saturating_sub(memory_mb),
            cpu_limit_pct: if quota > 0 {
                (quota * 100 / period.max(1)) as u32
            } else {
                0
            },
            disk_gb,
        },
        image,
    ))
}

async fn get_hardware(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let hw = lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        // The rootfs volume lives on the storage driver; this connection is
        // the LXC one, which reaches it through libvirtd all the same.
        Ok(read_hardware(&d, c)?.0)
    }))
    .await
    .map_err(ApiError::not_found)?;
    Ok(Json(json!(hw)))
}

/// Replace `<tag>...</tag>` (or `<tag .../>`) with `new`, or insert `new`
/// before `before` when there is none. `new` may be empty (remove).
fn set_element(xml: &str, tag: &str, new: &str, before: &str) -> String {
    if let Some(i) = xml
        .find(&format!("<{tag}>"))
        .or_else(|| xml.find(&format!("<{tag} ")))
    {
        let close = format!("</{tag}>");
        let end = match xml[i..].find(&close) {
            Some(j) => i + j + close.len(),
            None => i + xml[i..].find("/>").map_or(0, |j| j + 2),
        };
        return format!("{}{new}{}", &xml[..i], &xml[end..]);
    }
    match xml.find(before) {
        Some(i) if !new.is_empty() => format!("{}{new}{}", &xml[..i], &xml[i..]),
        _ => xml.to_string(),
    }
}

/// The definition with `hw`'s sizes in place of the current ones.
pub fn with_hardware(xml: &str, hw: &Hardware) -> String {
    let mut x = set_element(
        xml,
        "memory",
        &format!("<memory unit='MiB'>{}</memory>", hw.memory_mb),
        "<vcpu",
    );
    // currentMemory follows memory: a container has no balloon to size below it.
    x = set_element(&x, "currentMemory", "", "");
    let vcpu = format!("<vcpu placement='static'>{}</vcpu>", hw.vcpus);
    x = set_element(&x, "vcpu", &vcpu, "<os");
    let swap = if hw.swap_mb > 0 {
        format!(
            "<memtune><swap_hard_limit unit='MiB'>{}</swap_hard_limit></memtune>",
            hw.memory_mb + hw.swap_mb
        )
    } else {
        String::new()
    };
    x = set_element(&x, "memtune", &swap, "<vcpu");
    let tune = if hw.cpu_limit_pct > 0 {
        format!(
            "<cputune><period>100000</period><quota>{}</quota></cputune>",
            u64::from(hw.cpu_limit_pct) * 1000
        )
    } else {
        String::new()
    };
    set_element(&x, "cputune", &tune, "<os")
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

/// Change a container's resources. Measured on libvirt 12.7, the LXC driver
/// applies a CPU cap and a *lower* memory limit to a running container at
/// once, through its cgroup; more memory than it started with, the vCPU
/// count and swap cannot change live, so they are written to the definition and reported as
/// `pending` until the next start rather than claimed. A bigger root disk is
/// grown now, running or not: the volume, its loop device, then ext4 online.
async fn put_hardware(
    Path(name): Path<String>,
    Json(hw): Json<Hardware>,
) -> Result<Json<Value>, ApiError> {
    check_hardware(&hw).map_err(ApiError::bad_request)?;
    let v = lxc(with_conn(move |c| {
        let d = crate::libvirt::domain(c, &name)?;
        let (cur, image) = read_hardware(&d, c)?;
        if hw.disk_gb < cur.disk_gb {
            return Err(format!(
                "the root disk can only grow ({} GB now); shrinking it could lose data",
                cur.disk_gb
            )
            .into());
        }
        let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?;
        let next = with_hardware(&xml, &hw);
        ct::same_rootfs(&xml, &next, &d.get_uuid_string()?)?;
        Domain::define_xml(c, &next)?;

        let running = d.is_active().unwrap_or(false);
        let mut applied = vec![];
        let mut pending = vec![];
        if running {
            if hw.memory_mb != cur.memory_mb {
                // Measured: lowering applies to the running container's
                // cgroup at once; raising past what it started with is
                // refused ("Cannot resize the max memory on an active
                // domain"), so that is pending until the next start.
                let kib = hw.memory_mb * 1024;
                match d.set_memory_flags(kib, virt::sys::VIR_DOMAIN_AFFECT_LIVE) {
                    Ok(_) => applied.push("memory"),
                    Err(_) => pending.push("memory"),
                }
            }
            if hw.cpu_limit_pct != cur.cpu_limit_pct {
                // The quota is enforced by the cgroup, so it applies now.
                let quota = if hw.cpu_limit_pct > 0 {
                    i64::from(hw.cpu_limit_pct) * 1000
                } else {
                    -1
                };
                let info = virt::domain::SchedulerInfo {
                    scheduler_type: "posix".into(),
                    vcpu_bw: virt::domain::SchedBandwidth {
                        period: Some(100_000),
                        quota: Some(quota),
                    },
                    ..Default::default()
                };
                let ok = d
                    .set_scheduler_parameters_flags(&info, virt::sys::VIR_DOMAIN_AFFECT_LIVE)
                    .is_ok();
                if ok {
                    applied.push("cpu limit")
                } else {
                    pending.push("cpu limit")
                }
            }
            if hw.vcpus != cur.vcpus {
                pending.push("vCPUs");
            }
            if hw.swap_mb != cur.swap_mb {
                pending.push("swap");
            }
        }
        let mut grown = false;
        if hw.disk_gb > cur.disk_gb {
            let vol = StorageVol::lookup_by_path(c, &image)?;
            vol.resize(hw.disk_gb << 30, 0)?;
            ct::grow_fs(&image)?;
            grown = true;
        }
        Ok(json!({
            "name": name, "running": running, "applied": applied, "pending": pending,
            "disk_grown": grown, "hardware": hw,
        }))
    }))
    .await
    .map_err(ApiError::bad_request)?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_are_checked_and_derived() {
        assert!(check_hostname("web-1").is_ok());
        for bad in ["", "-a", "a-", "a b", "a.b", "a;rm", &"x".repeat(64)] {
            assert!(check_hostname(bad).is_err(), "{bad}");
        }
        assert_eq!(hostname_from("My Web Server!"), "my-web-server");
        assert_eq!(hostname_from("***"), "ct");
        assert!(check_hostname(&hostname_from(&"é".repeat(80))).is_ok());
    }

    #[test]
    fn hardware_is_rewritten_in_place_and_read_back() {
        let nets = vec!["internet".to_string()];
        let xml = ct::domain_xml(&ct::Spec {
            uuid: "u",
            title: "t",
            memory_mb: 512,
            swap_mb: 256,
            vcpus: 1,
            rootfs: "/p/u.raw",
            networks: &nets,
            id_base: Some(ct::ID_BASE),
        });
        let hw = Hardware {
            vcpus: 4,
            memory_mb: 2048,
            swap_mb: 0,
            cpu_limit_pct: 150,
            disk_gb: 8,
        };
        let x = with_hardware(&xml, &hw);
        assert_eq!(mib_of(&x, "memory"), 2048);
        assert!(x.contains("<vcpu placement='static'>4</vcpu>"), "{x}");
        assert!(!x.contains("<memtune>"), "swap 0 drops the limit");
        assert!(x.contains("<quota>150000</quota>"), "{x}");
        assert_eq!(x.matches("<vcpu").count(), 1, "replaced, not added");
        assert!(
            ct::same_rootfs(&xml, &x, "u").is_ok(),
            "the rootfs is untouched"
        );
        let back = with_hardware(
            &x,
            &Hardware {
                cpu_limit_pct: 0,
                swap_mb: 64,
                ..hw.clone()
            },
        );
        assert!(
            !back.contains("<cputune>") && back.contains("<swap_hard_limit unit='MiB'>2112<"),
            "{back}"
        );
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
    fn uuids_are_canonical_v4() {
        let u = new_uuid();
        assert!(crate::libvirt::is_uuid(&u), "{u}");
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, new_uuid());
    }
}
