//! `/pools/{pool}/volumes`: the files in a directory pool.
//!
//! A volume is a regular file directly inside a pool's root. Nothing is cached
//! and nothing is registered: list is a directory scan, so a file dropped in
//! by `cp` or a sync is visible at once, and one removed with `rm` disappears.
//!
//! The wire shape is the one the SPA has always read (`name`, `title`, `path`,
//! `capacity`, `allocation`, `used_by`, `content`), but the operations are
//! plain files: a raw volume is a sparse file, a qcow2 one is made by
//! `qemu-img`, upload and download write through a temp file in the pool root
//! and rename it into place, and no operation ever writes through a symlink.

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, Query};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;

use super::ApiError;
use crate::disks::{self, DiskSource};
use crate::libvirt::with_conn;
use crate::pools::{Pools, safe_name};

pub fn routes() -> Router {
    Router::new()
        .route("/pools/{pool}/volumes", get(list).post(create))
        .route("/pools/{pool}/volumes/{vol}", get(get_one).delete(delete))
        .route(
            "/pools/{pool}/volumes/{vol}/title",
            axum::routing::put(set_title),
        )
        .route("/pools/{pool}/volumes/{vol}/upload", post(upload))
        .route("/pools/{pool}/volumes/{vol}/hash", get(hash))
}

/// A volume lookup's failure as HTTP: missing is 404, a pool root that changed
/// is a conflict the administrator must fix, anything else is the request.
fn volume_error(e: String) -> ApiError {
    if e.starts_with("no volume '") {
        ApiError::not_found(e)
    } else if e.contains("changed or is a symlink") || e.contains("start the pool") {
        ApiError::conflict(e)
    } else {
        ApiError::bad_request(e)
    }
}

fn key_of(pools: &Pools, pool: &str) -> Result<String, ApiError> {
    pools
        .resolve_key(pool)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{pool}'")))
}

async fn list(
    Extension(pools): Extension<Pools>,
    Path(pool): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    let vols = pools.scan(&key);
    // Which guests hold each volume, so a picker can say "in use by" and not
    // offer one guest's disk to another. One pass over the domains.
    let held = {
        let pools = pools.clone();
        with_conn(move |c| Ok(disks::holders_by_label(c, &pools)))
            .await
            .map_err(ApiError::internal)?
    };
    let arr: Vec<Value> = vols
        .iter()
        .map(|v| {
            let path = v.path.to_string_lossy();
            json!({
                "name": v.name,
                "title": crate::volume_titles::get(&key, &v.name),
                "path": path,
                "capacity": v.capacity,
                "allocation": v.allocation,
                "used_by": disks::holders_of(&held, &key, &v.name, &path),
                // iso / vztmpl / images, so the pool page can split them.
                "content": super::templates::content_of(&v.name),
            })
        })
        .collect();
    Ok(Json(json!(arr)))
}

async fn get_one(
    Extension(pools): Extension<Pools>,
    Path((pool, vol)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    let v = pools
        .volume(&key, &vol)
        .ok_or_else(|| ApiError::not_found(format!("no volume '{vol}' in {pool}")))?;
    Ok(Json(json!({
        "name": v.name,
        "title": crate::volume_titles::get(&key, &v.name),
        "path": v.path.to_string_lossy(),
        "capacity": v.capacity,
        "allocation": v.allocation,
    })))
}

#[derive(Deserialize)]
struct VolCreate {
    xml: String,
}

#[derive(Debug, PartialEq, Eq)]
struct VolSpec {
    name: String,
    capacity: u64,
    format: String,
}

/// The opening tag `<(tag) ...>`, for reading its attributes.
fn open_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let i = xml.find(&format!("<{tag}"))?;
    let t = &xml[i..];
    Some(&t[..t.find('>')? + 1])
}

/// The multiplier for a libvirt `<capacity unit=...>`. A missing unit is bytes,
/// which is what `volumeXml` in the SPA writes.
fn capacity_unit(unit: &str) -> Result<u64, String> {
    Ok(match unit {
        "B" | "bytes" => 1,
        "KiB" | "K" => 1 << 10,
        "MiB" | "M" => 1 << 20,
        "GiB" | "G" => 1 << 30,
        "TiB" | "T" => 1 << 40,
        other => return Err(format!("unknown capacity unit '{other}'")),
    })
}

/// `<volume><name>..</name><capacity unit='..'>..</capacity><target><format
/// type='raw|qcow2'/></target></volume>`, as `volumeXml` in the SPA writes it
/// (bytes), with libvirt's units accepted too.
fn parse_volume_xml(xml: &str) -> Result<VolSpec, String> {
    let tag = |tag: &str| -> Option<String> {
        let open = open_tag(xml, tag)?;
        let i = xml.find(open)? + open.len();
        let rest = &xml[i..];
        let end = rest.find(&format!("</{tag}>"))?;
        Some(crate::libvirt::unescape(rest[..end].trim()))
    };
    let name = tag("name").ok_or("the volume definition has no <name>")?;
    let name = safe_name(&name)?.to_string();
    let count: u64 = tag("capacity")
        .ok_or("the volume definition has no <capacity>")?
        .parse()
        .map_err(|_| "<capacity> must be a number".to_string())?;
    let unit = open_tag(xml, "capacity")
        .and_then(|t| disks::attr(t, "unit"))
        .unwrap_or("B");
    let capacity = count
        .checked_mul(capacity_unit(unit)?)
        .filter(|c| *c > 0)
        .ok_or("capacity must be more than 0 bytes")?;
    let format = match xml.find("<format") {
        Some(i) => {
            let t = &xml[i..];
            let open = &t[..t.find('>').map(|e| e + 1).unwrap_or(t.len())];
            disks::attr(open, "type").unwrap_or("raw").to_string()
        }
        None => "raw".to_string(),
    };
    if format != "raw" && format != "qcow2" {
        return Err(format!("unknown volume format '{format}'"));
    }
    Ok(VolSpec {
        name,
        capacity,
        format,
    })
}

/// Make the volume's file. A raw volume is sparse (`set_len`, no data blocks);
/// a qcow2 one is made by `qemu-img`, which knows the header. A failure
/// removes a partial file so a refused create leaves nothing counting against
/// a quota.
fn create_volume(pools: &Pools, pool: &str, spec: &VolSpec) -> Result<PathBuf, String> {
    if spec.format == "raw" {
        return pools.create_raw(pool, &spec.name, spec.capacity);
    }
    let p = pools.get(pool).ok_or_else(|| format!("no pool '{pool}'"))?;
    if !p.active {
        return Err(format!("pool '{pool}' is not active"));
    }
    let root = pools.root(pool)?;
    let name = safe_name(&spec.name)?;
    let path = root.join(name);
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(format!("a volume named '{name}' already exists"));
    }
    // Pre-create 0600 so qemu-img's own 0644 default never lands on disk.
    // Current qemu-img writes into the file it is given; one that refuses an
    // existing destination is handled by the temp-and-rename fallback below.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    if let Err(first) = qemu_img_create(&path, spec.capacity) {
        let _ = std::fs::remove_file(&path);
        let temp = root.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        match qemu_img_create(&temp, spec.capacity) {
            Ok(()) => {
                let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
                if let Err(e) = std::fs::rename(&temp, &path) {
                    let _ = std::fs::remove_file(&temp);
                    return Err(format!("{}: {e}", path.display()));
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                let _ = first;
                return Err(e);
            }
        }
    }
    // Some qemu-img versions recreate the destination with their own mode.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

fn qemu_img_create(path: &FsPath, capacity: u64) -> Result<(), String> {
    let out = std::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(capacity.to_string())
        .output()
        .map_err(|e| {
            format!("qemu-img is needed to create a qcow2 volume (install qemu-utils): {e}")
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "qemu-img create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

async fn create(
    Extension(pools): Extension<Pools>,
    Path(pool): Path<String>,
    Json(body): Json<VolCreate>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    let spec = parse_volume_xml(&body.xml).map_err(ApiError::bad_request)?;
    let name = spec.name.clone();
    let path = tokio::task::spawn_blocking(move || create_volume(&pools, &key, &spec))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({"name": name, "path": path.to_string_lossy()})))
}

#[derive(serde::Deserialize)]
struct TitleReq {
    title: String,
}

async fn set_title(
    Extension(pools): Extension<Pools>,
    Path((pool, vol)): Path<(String, String)>,
    Json(req): Json<TitleReq>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    pools.checked_volume(&key, &vol).map_err(volume_error)?;
    crate::volume_titles::set(&key, &vol, &req.title).map_err(ApiError::internal)?;
    Ok(Json(json!({"name": vol, "title": req.title.trim()})))
}

async fn delete(
    Extension(pools): Extension<Pools>,
    Path((pool, vol)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    // Refuse while any guest still has this disk attached: deleting it would
    // corrupt that guest's next boot, and the guest would look fine until it
    // was restarted. Checked in its own connection so the refusal can be a
    // 409 with the offending guest names rather than a blanket 500.
    let target = DiskSource::Volume {
        pool: key.clone(),
        volume: vol.clone(),
    };
    let holders = {
        let pools = pools.clone();
        with_conn(move |c| Ok(disks::referencing_domains(c, &pools, &target)))
            .await
            .map_err(ApiError::internal)?
    };
    if !holders.is_empty() {
        return Err(ApiError::conflict(format!(
            "'{vol}' is in use by {} — detach it first",
            holders.join(", ")
        )));
    }
    let path = pools.checked_volume(&key, &vol).map_err(volume_error)?.path;
    std::fs::remove_file(&path)
        .map_err(|e| ApiError::internal(format!("{}: {e}", path.display())))?;
    // The file is gone, which is what the caller asked for; a stale title is
    // clutter, not a reason to report the delete as failed.
    if let Err(e) = crate::volume_titles::set(&key, &vol, "") {
        tracing::warn!("volume {key}/{vol}: title not cleared: {e}");
    }
    Ok(Json(json!({"name": vol, "deleted": true})))
}

/// Lowercase hex of a digest.
pub(super) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file, streamed in chunks so a multi-GB ISO is never held in
/// memory. Used by `GET .../hash` to verify a volume that already exists —
/// the upload path hashes while it copies instead.
///
/// The digest is of the stored bytes, so for a qcow2 volume it is the digest
/// of that container, not of the guest's view of the disk. Uploads create raw
/// volumes, where the two coincide.
fn sha256_file(path: &FsPath) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((to_hex(&hasher.finalize()), total))
}

/// Content hash and size of a stored volume.
async fn hash(
    Extension(pools): Extension<Pools>,
    Path((pool, vol)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    let v = pools
        .volume(&key, &vol)
        .ok_or_else(|| ApiError::not_found(format!("no volume '{vol}' in {pool}")))?;
    let path = v.path.clone();
    let (sha256, size) = tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({"name": v.name, "sha256": sha256, "size": size}),
    ))
}

/// Scratch space for a spooled upload body.
///
/// Deliberately **not** `std::env::temp_dir()`: `/tmp` is tmpfs on this host, so
/// spooling an 8 GB ISO there would hold the whole thing in RAM. `/var/tmp` is
/// disk-backed by convention. `VIRT_MAN_SPOOL_DIR` overrides it (the directory
/// must already exist).
pub(super) fn spool_dir() -> PathBuf {
    std::env::var_os("VIRT_MAN_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

/// A spool path unique to this request. Two uploads naming the same volume used
/// to share one file, so concurrent uploads corrupted each other.
fn spool_path(pool: &str, vol: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    spool_dir().join(format!(
        "virtman-{pool}-{vol}-{}-{n}.upload",
        std::process::id()
    ))
}

/// Bytes per second, guarding the divide. A transfer fast enough to register
/// zero elapsed time would otherwise report an infinite or NaN rate.
fn bps(bytes: u64, secs: f64) -> f64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0.0;
    }
    bytes as f64 / secs
}

async fn spool_body(body: Body, tmp: &FsPath) -> Result<u64, ApiError> {
    let mut file = tokio::fs::File::create(tmp)
        .await
        .map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    loop {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                let _ = std::fs::remove_file(tmp);
                return Err(ApiError::bad_request(e));
            }
            None => break,
        };
        if let Err(e) = file.write_all(&chunk).await {
            let _ = std::fs::remove_file(tmp);
            return Err(ApiError::internal(e));
        }
        total += chunk.len() as u64;
    }
    if let Err(e) = file.flush().await {
        let _ = std::fs::remove_file(tmp);
        return Err(ApiError::internal(e));
    }
    Ok(total)
}

/// Copy `src` into `dst`, hashing as it goes.
fn copy_hashed(src: &FsPath, dst: &mut std::fs::File) -> std::io::Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let mut input = std::fs::File::open(src)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, to_hex(&hasher.finalize())))
}

/// Why placing a spool failed, split by who can fix it: a conflict (state the
/// caller must change), a bad request, or an internal filesystem error.
#[derive(Debug)]
pub(super) enum PlaceError {
    Conflict(String),
    BadRequest(String),
    Internal(String),
}

impl std::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaceError::Conflict(m) | PlaceError::BadRequest(m) | PlaceError::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl PlaceError {
    fn into_api(self) -> ApiError {
        match self {
            PlaceError::Conflict(m) => ApiError::conflict(m),
            PlaceError::BadRequest(m) => ApiError::bad_request(m),
            PlaceError::Internal(m) => ApiError::internal(m),
        }
    }
}

/// Put a completed spool file into a pool as `name`, atomically.
///
/// Written to a temp file in the pool root, hashed while copying, fsynced and
/// only then renamed over the target. A failure — a short read, a rejected
/// digest, a full disk — removes the temp and leaves an existing target
/// exactly as it was, which is what makes overwriting a volume safe. The rename
/// is atomic within one directory, so a reader never sees half a file under the
/// real name.
///
/// `overwrite` false refuses anything already at that name, including a
/// symlink or a directory.
pub(super) fn place_spooled(
    pools: &Pools,
    pool: &str,
    name: &str,
    src: &FsPath,
    overwrite: bool,
    expect_sha: Option<&str>,
) -> Result<(u64, String), PlaceError> {
    let p = pools
        .get(pool)
        .ok_or_else(|| PlaceError::BadRequest(format!("no pool '{pool}'")))?;
    if !p.active {
        return Err(PlaceError::Conflict(format!("pool '{pool}' is not active")));
    }
    let root = pools.root(pool).map_err(PlaceError::Conflict)?;
    let name = safe_name(name).map_err(PlaceError::BadRequest)?;
    let target = root.join(name);
    match std::fs::symlink_metadata(&target) {
        Ok(m) => {
            if !overwrite {
                return Err(PlaceError::Conflict(format!(
                    "a volume named '{name}' already exists"
                )));
            }
            if !m.file_type().is_file() {
                return Err(PlaceError::BadRequest(format!(
                    "'{name}' is not a regular file"
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PlaceError::Internal(format!("{}: {e}", target.display()))),
    }
    let temp = root.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| PlaceError::Internal(format!("{}: {e}", temp.display())))?;
    let copied = copy_hashed(src, &mut out);
    let outcome: Result<(u64, String), PlaceError> = match copied {
        Err(e) => Err(PlaceError::Internal(format!("{}: {e}", temp.display()))),
        Ok((total, sha)) => match expect_sha {
            Some(want) if want != sha => Err(PlaceError::BadRequest(format!(
                "sha256 mismatch: expected {want}, got {sha}"
            ))),
            _ => match out.sync_all() {
                Err(e) => Err(PlaceError::Internal(format!("{}: {e}", temp.display()))),
                Ok(()) => Ok((total, sha)),
            },
        },
    };
    drop(out);
    let outcome = match outcome {
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        Ok(v) => v,
    };
    if let Err(e) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(PlaceError::Internal(format!("{}: {e}", target.display())));
    }
    Ok(outcome)
}

/// Stream a body into a volume. `?name=` creates a new raw file; otherwise the
/// body replaces the named existing one.
async fn upload(
    Extension(pools): Extension<Pools>,
    Path((pool, vol)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let key = key_of(&pools, &pool)?;
    let p = pools.get(&key).expect("resolved pool is present");
    if !p.active {
        return Err(ApiError::conflict(format!("pool '{pool}' is not active")));
    }
    // A root replaced by a symlink is refused with its own reason, before the
    // body is accepted.
    pools.root(&key).map_err(ApiError::conflict)?;
    let create_name = q.get("name").cloned();
    let name = match &create_name {
        Some(n) => {
            safe_name(n).map_err(ApiError::bad_request)?;
            if pools.entry_exists(&key, n) {
                return Err(ApiError::conflict(format!(
                    "a volume named '{n}' already exists in {pool}"
                )));
            }
            n.clone()
        }
        None => {
            pools.checked_volume(&key, &vol).map_err(volume_error)?;
            vol.clone()
        }
    };

    let tmp = spool_path(&key, &name);
    // Measured separately because they fail differently: a slow receive means
    // the network, a slow write means the storage. Reporting one number would
    // hide which end is the bottleneck.
    let recv_start = std::time::Instant::now();
    let total = spool_body(body, &tmp).await?;
    let recv_secs = recv_start.elapsed().as_secs_f64();

    let write_start = std::time::Instant::now();
    let placed = {
        let pools = pools.clone();
        let key = key.clone();
        let name = name.clone();
        let spool = tmp.clone();
        tokio::task::spawn_blocking(move || {
            place_spooled(&pools, &key, &name, &spool, create_name.is_none(), None)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    };
    let _ = std::fs::remove_file(&tmp);
    let (_, sha256) = placed.map_err(PlaceError::into_api)?;
    let write_secs = write_start.elapsed().as_secs_f64();
    Ok(Json(json!({
        "name": name,
        "uploaded": total,
        "sha256": sha256,
        "recv_bps": bps(total, recv_secs),
        "write_bps": bps(total, write_secs),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-pools");
        let _ = std::fs::create_dir_all(&base);
        let dir = base.join(format!("vol-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const POOL: &str = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";

    fn fixture(dir: &FsPath) -> (Pools, PathBuf) {
        let pools = Pools::load(dir.join("state/pools.json")).unwrap();
        let root = dir.join("images");
        let path = pools.prepare_root(root.to_str().unwrap(), None).unwrap();
        let mut s = pools.lock();
        s.pools.insert(
            POOL.into(),
            crate::pools::Pool {
                title: "test".into(),
                path: path.clone(),
                autostart: true,
                active: true,
            },
        );
        pools.save(&s).unwrap();
        drop(s);
        (pools, path)
    }

    #[test]
    fn volume_xml_is_parsed_with_its_capacity_and_format() {
        let xml = "<volume>\n  <name>disk.qcow2</name>\n  <capacity>1073741824</capacity>\n  <target><format type=\"qcow2\"/></target>\n</volume>";
        assert_eq!(
            parse_volume_xml(xml).unwrap(),
            VolSpec {
                name: "disk.qcow2".into(),
                capacity: 1 << 30,
                format: "qcow2".into()
            }
        );
        assert_eq!(
            parse_volume_xml(&xml.replace("qcow2", "raw"))
                .unwrap()
                .format,
            "raw"
        );
        assert!(
            parse_volume_xml(&xml.replace("qcow2", "vmdk")).is_err(),
            "unknown formats are refused"
        );
        assert!(
            parse_volume_xml("<volume><name>../x</name><capacity>1</capacity></volume>").is_err(),
            "a name is one component"
        );
        assert!(
            parse_volume_xml("<volume><name>x</name><capacity>0</capacity></volume>").is_err(),
            "a zero-byte volume is refused"
        );
    }

    #[test]
    fn capacity_units_follow_libvirts_multipliers() {
        let with = |unit: &str| {
            format!(
                "<volume><name>x</name><capacity unit='{unit}'>2</capacity>\
                 <target><format type='raw'/></target></volume>"
            )
        };
        let cap = |xml: &str| parse_volume_xml(xml).unwrap().capacity;
        assert_eq!(cap(&with("GiB")), 2 << 30);
        assert_eq!(cap(&with("MiB")), 2 << 20);
        assert_eq!(cap(&with("KiB")), 2 << 10);
        assert_eq!(cap(&with("B")), 2);
        assert_eq!(
            cap("<volume><name>x</name><capacity>5</capacity></volume>"),
            5,
            "a missing unit is bytes, which is what the SPA writes"
        );
        assert!(
            parse_volume_xml(&with("PB")).is_err(),
            "an unknown unit is refused rather than guessed"
        );
        assert!(
            parse_volume_xml(&format!(
                "<volume><name>x</name><capacity unit='GiB'>{}</capacity></volume>",
                u64::MAX
            ))
            .is_err(),
            "an overflowing capacity is refused"
        );
    }

    #[test]
    fn a_raw_volume_is_sparse_and_a_second_create_is_refused() {
        let dir = scratch("raw");
        let (pools, root) = fixture(&dir);
        let spec = VolSpec {
            name: "disk.raw".into(),
            capacity: 1 << 30,
            format: "raw".into(),
        };
        let path = create_volume(&pools, POOL, &spec).unwrap();
        assert_eq!(path, root.join("disk.raw"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1 << 30);
        assert!(create_volume(&pools, POOL, &spec).is_err());
    }

    /// Skipped, with a message, on a host without `qemu-img`: the point is the
    /// header's virtual size, not the tool.
    #[test]
    fn a_qcow2_volume_reports_its_virtual_size() {
        if std::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: qemu-img is not installed");
            return;
        }
        let dir = scratch("qcow2");
        let (pools, _) = fixture(&dir);
        let spec = VolSpec {
            name: "disk.qcow2".into(),
            capacity: 2 << 30,
            format: "qcow2".into(),
        };
        let path = create_volume(&pools, POOL, &spec).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "qemu-img's own 0644 default never lands");
        let v = pools.volume(POOL, "disk.qcow2").unwrap();
        assert_eq!(v.capacity, 2 << 30, "the header, not the file length");
        assert!(
            v.allocation < v.capacity,
            "a fresh qcow2 is sparse: {} allocated",
            v.allocation
        );
    }

    #[test]
    fn a_spool_is_placed_atomically_and_failures_leave_the_target_alone() {
        let dir = scratch("place");
        let (pools, root) = fixture(&dir);
        let target = root.join("disk.iso");
        std::fs::write(&target, b"old").unwrap();
        let spool = dir.join("spool");
        std::fs::write(&spool, b"new bytes").unwrap();

        // A rejected digest happens before the rename: the old file survives.
        let err = place_spooled(
            &pools,
            POOL,
            "disk.iso",
            &spool,
            true,
            Some(&"0".repeat(64)),
        )
        .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(
            !std::fs::read_dir(&root).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".disk.iso")
            }),
            "no temp file survives a failure"
        );

        // Without overwrite an existing target is refused.
        assert!(place_spooled(&pools, POOL, "disk.iso", &spool, false, None).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");

        // The success path replaces it whole and reports the digest.
        let (size, sha) = place_spooled(&pools, POOL, "disk.iso", &spool, true, None).unwrap();
        assert_eq!(size, 9);
        assert_eq!(std::fs::read(&target).unwrap(), b"new bytes");
        assert_eq!(sha, sha256_file(&target).unwrap().0);
    }

    #[test]
    fn place_errors_are_classified_by_who_can_fix_them() {
        let dir = scratch("place-errors");
        let (pools, root) = fixture(&dir);
        let spool = dir.join("spool");
        std::fs::write(&spool, b"new").unwrap();
        std::fs::write(root.join("exists.raw"), b"old").unwrap();

        let e = place_spooled(&pools, POOL, "exists.raw", &spool, false, None).unwrap_err();
        assert!(matches!(e, PlaceError::Conflict(_)), "{e:?}");

        std::fs::create_dir(root.join("a-dir")).unwrap();
        let e = place_spooled(&pools, POOL, "a-dir", &spool, true, None).unwrap_err();
        assert!(matches!(e, PlaceError::BadRequest(_)), "{e:?}");

        pools.lock().pools.get_mut(POOL).unwrap().active = false;
        let e = place_spooled(&pools, POOL, "new.raw", &spool, false, None).unwrap_err();
        assert!(matches!(e, PlaceError::Conflict(_)), "{e:?}");
        pools.lock().pools.get_mut(POOL).unwrap().active = true;

        // A root hijacked by a symlink is a conflict with its own reason.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::rename(&root, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        let e = place_spooled(&pools, POOL, "new.raw", &spool, false, None).unwrap_err();
        assert!(matches!(e, PlaceError::Conflict(_)), "{e:?}");
        assert!(e.to_string().contains("re-point the pool"), "{e}");
        assert!(
            !elsewhere.join("new.raw").exists(),
            "nothing written outside"
        );
    }

    #[test]
    fn traversal_never_escapes_the_pool() {
        let dir = scratch("traverse");
        let (pools, _) = fixture(&dir);
        let spool = dir.join("spool");
        std::fs::write(&spool, b"x").unwrap();
        for bad in ["../outside", "a/b", "..", "."] {
            assert!(
                place_spooled(&pools, POOL, bad, &spool, false, None).is_err(),
                "{bad}"
            );
            assert!(!dir.join("outside").exists());
        }
    }

    #[test]
    fn a_symlinked_target_is_not_overwritten() {
        let dir = scratch("symlink");
        let (pools, root) = fixture(&dir);
        let outside = dir.join("host-file");
        std::fs::write(&outside, b"host").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.iso")).unwrap();
        let spool = dir.join("spool");
        std::fs::write(&spool, b"new").unwrap();
        assert!(place_spooled(&pools, POOL, "link.iso", &spool, true, None).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"host");
    }

    #[test]
    fn sha256_file_matches_a_known_digest() {
        let dir = scratch("hash");
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        let (hex, size) = sha256_file(&path).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_streams_past_one_buffer() {
        let dir = scratch("hash-big");
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![7u8; 200 * 1024]).unwrap();
        let (hex, size) = sha256_file(&path).unwrap();
        assert_eq!(size, 200 * 1024);
        assert_eq!(hex.len(), 64, "full width hex digest");
    }

    #[test]
    fn an_empty_file_hashes_to_the_empty_digest() {
        let dir = scratch("hash-empty");
        let path = dir.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        let (hex, size) = sha256_file(&path).unwrap();
        assert_eq!(size, 0);
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn bps_never_divides_by_zero() {
        assert_eq!(bps(1000, 0.0), 0.0);
        assert_eq!(bps(1000, -1.0), 0.0);
        assert_eq!(bps(1000, f64::NAN), 0.0);
    }

    #[test]
    fn bps_is_bytes_per_second() {
        assert_eq!(bps(1000, 2.0), 500.0);
        assert_eq!(bps(0, 3.0), 0.0);
    }

    /// Through the real router: a create returns `{name,path}` and the volume
    /// reads back. These two routes never touch libvirt.
    #[tokio::test]
    async fn the_routes_create_and_read_a_raw_volume() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = scratch("routes");
        let (pools, root) = fixture(&dir);
        let app = || super::routes().layer(Extension(pools.clone()));
        let call = |req: Request<Body>| {
            let app = app();
            async move {
                let r = app.oneshot(req).await.unwrap();
                let status = r.status();
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
                )
            }
        };

        let xml = format!(
            "<volume><name>disk.raw</name><capacity>{}</capacity>\
             <target><format type='raw'/></target></volume>",
            1 << 20
        );
        let (status, made) = call(
            Request::post(format!("/pools/{POOL}/volumes"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "xml": xml }).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200, "{made}");
        assert_eq!(made["name"], "disk.raw");
        assert_eq!(
            made["path"],
            root.join("disk.raw").to_string_lossy().as_ref()
        );

        let (status, got) = call(
            Request::get(format!("/pools/{POOL}/volumes/disk.raw"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["name"], "disk.raw");
        assert_eq!(got["capacity"], 1 << 20);

        let (status, _) = call(
            Request::get(format!("/pools/{POOL}/volumes/missing.raw"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, 404, "a missing volume is not found");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
