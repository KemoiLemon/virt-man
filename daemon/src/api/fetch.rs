//! Download an ISO from a URL straight into a storage pool.
//!
//! The daemon fetches it itself, so the bytes never pass through the web
//! server or the browser: a multi-GB image costs one transfer, not three, and
//! closing the tab does not stop it. Jobs live in memory and are listed with
//! `GET /fetches`; the pool page polls that for progress.
//!
//! The download spools to disk first and is written into the volume only when
//! it arrived in full, through the same hash-while-writing path as an upload.
//! Writing straight into the volume would leave a truncated volume behind a
//! dropped connection, looking like a finished one. A server that sent
//! `Content-Length` must deliver exactly that many bytes. When the caller gives
//! a SHA-256 (distributions publish one), the spooled bytes must match it
//! before anything is written, so a corrupted or swapped image never reaches
//! the pool.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Extension, Path};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::ApiError;
use crate::pools::Pools;

/// A finished job stays listed this long, then is dropped (the activity log
/// in the web UI is the durable record).
const FINISHED_TTL_MS: u64 = 15 * 60 * 1000;
/// Give up on a server that sends nothing for this long.
const STALL_SECS: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Downloading,
    Verifying,
    Writing,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub pool: String,
    pub name: String,
    pub url: String,
    pub phase: Phase,
    /// Bytes received so far, and the total if the server said.
    pub received: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
    pub sha256: Option<String>,
    pub error: Option<String>,
    pub started_ms: u64,
    pub finished_ms: u64,
    #[serde(skip)]
    cancel: bool,
}

type Jobs = Arc<Mutex<HashMap<String, Job>>>;

fn jobs() -> &'static Jobs {
    static JOBS: OnceLock<Jobs> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

pub fn routes() -> Router {
    Router::new()
        .route("/fetches", get(list))
        .route("/pools/{pool}/fetch", post(start))
        .route("/fetches/{id}", delete(cancel))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct StartReq {
    url: String,
    /// Volume name. Defaults to the URL's last path segment.
    #[serde(default)]
    name: Option<String>,
    /// Expected SHA-256 (hex). Checked before anything is written.
    #[serde(default)]
    sha256: Option<String>,
}

/// Validate the request: an http(s) URL, a usable file name, a well-formed
/// digest. Returns (url, name, lowercase digest).
fn check(req: &StartReq) -> Result<(reqwest::Url, String, Option<String>), String> {
    let url = reqwest::Url::parse(req.url.trim()).map_err(|e| format!("not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https URLs can be fetched".into());
    }
    let name = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .ok_or("the URL has no file name; give the volume a name")?,
    };
    if name.contains('/') || name == "." || name == ".." || name.chars().any(char::is_control) {
        return Err(format!("'{name}' is not a usable volume name"));
    }
    let sha = match req
        .sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) => {
            Some(s.to_ascii_lowercase())
        }
        Some(_) => return Err("sha256 must be 64 hex characters".into()),
        None => None,
    };
    Ok((url, name, sha))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn start(
    Extension(pools): Extension<Pools>,
    Path(pool): Path<String>,
    Json(req): Json<StartReq>,
) -> Result<Json<Value>, ApiError> {
    let (url, name, sha) = check(&req).map_err(ApiError::bad_request)?;
    let job = begin(pools, pool, url, name, sha.map(Expect::Sha256)).await?;
    Ok(Json(serde_json::to_value(job).unwrap_or_default()))
}

/// The digest a download must match before it is written.
#[derive(Clone, Debug)]
pub enum Expect {
    Sha256(String),
}

/// Start a fetch into `pool` as `name`: spooled, complete, verified against
/// its published digest, and never overwriting an existing volume.
pub async fn begin(
    pools: Pools,
    pool: String,
    url: reqwest::Url,
    name: String,
    expect: Option<Expect>,
) -> Result<Job, ApiError> {
    // Refuse up front what would fail at the end, after gigabytes: a missing
    // or stopped pool, or a name that already exists (never overwritten).
    let key = pools
        .resolve_key(&pool)
        .ok_or_else(|| ApiError::bad_request(format!("no pool '{pool}'")))?;
    let p = pools.get(&key).expect("resolved pool is present");
    if !p.active {
        return Err(ApiError::conflict(format!("pool '{pool}' is not active")));
    }
    // The root itself is revalidated: a symlinked or replaced root is a
    // conflict the administrator fixes by re-pointing the pool.
    pools.root(&key).map_err(ApiError::conflict)?;
    if pools.entry_exists(&key, &name) {
        return Err(ApiError::conflict(format!(
            "a volume named '{name}' already exists in {pool}"
        )));
    }
    {
        let map = jobs().lock().expect("fetch jobs poisoned");
        if map
            .values()
            .any(|j| j.pool == pool && j.name == name && is_live(j.phase))
        {
            return Err(ApiError::conflict(format!(
                "'{name}' is already being fetched into {pool}"
            )));
        }
    }

    let id = format!("{:016x}", rand_u64());
    let job = Job {
        id: id.clone(),
        pool,
        name,
        url: url.to_string(),
        phase: Phase::Downloading,
        received: 0,
        total: None,
        speed_bps: 0.0,
        sha256: None,
        error: None,
        started_ms: now_ms(),
        finished_ms: 0,
        cancel: false,
    };
    jobs()
        .lock()
        .expect("fetch jobs poisoned")
        .insert(id.clone(), job.clone());
    tokio::spawn(async move {
        let result = run(&pools, &id, url, expect).await;
        let mut map = jobs().lock().expect("fetch jobs poisoned");
        if let Some(j) = map.get_mut(&id) {
            j.finished_ms = now_ms();
            match result {
                Ok(digest) => {
                    j.phase = Phase::Done;
                    j.sha256 = Some(digest);
                }
                Err(_) if j.cancel => j.phase = Phase::Cancelled,
                Err(e) => {
                    j.phase = Phase::Failed;
                    j.error = Some(e);
                }
            }
        }
    });
    Ok(job)
}

fn is_live(p: Phase) -> bool {
    matches!(p, Phase::Downloading | Phase::Verifying | Phase::Writing)
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    // /dev/urandom is always there on the hypervisor; a clock fallback still
    // yields a distinct id per job, which is all this is for.
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        return now_ms() ^ (std::process::id() as u64) << 40;
    }
    u64::from_le_bytes(b)
}

fn update(id: &str, f: impl FnOnce(&mut Job)) -> bool {
    let mut map = jobs().lock().expect("fetch jobs poisoned");
    match map.get_mut(id) {
        Some(j) => {
            f(j);
            !j.cancel
        }
        None => false,
    }
}

/// The whole job: download to a spool file, check it, write it into a new
/// volume. Returns the SHA-256 of what was written.
async fn run(
    pools: &Pools,
    id: &str,
    url: reqwest::Url,
    expect: Option<Expect>,
) -> Result<String, String> {
    let spool = super::volumes::spool_dir().join(format!("virtman-fetch-{id}.part"));
    let r = download(id, url, &spool).await;
    let out = match r {
        Ok((size, digest)) => finish(pools, id, &spool, size, &digest, expect).await,
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&spool).await;
    out
}

async fn download(
    id: &str,
    url: reqwest::Url,
    spool: &std::path::Path,
) -> Result<(u64, String), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("virt-man-daemon/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("the server answered {}", resp.status()));
    }
    let total = resp.content_length().filter(|&n| n > 0);
    update(id, |j| j.total = total);

    let mut file = tokio::fs::File::create(spool)
        .await
        .map_err(|e| format!("spool: {e}"))?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let started = std::time::Instant::now();
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(STALL_SECS), stream.next())
            .await
            .map_err(|_| format!("the server sent nothing for {STALL_SECS} s"))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("spool: {e}"))?;
        hasher.update(&chunk);
        got += chunk.len() as u64;
        let secs = started.elapsed().as_secs_f64();
        let alive = update(id, |j| {
            j.received = got;
            j.speed_bps = if secs > 0.0 { got as f64 / secs } else { 0.0 };
        });
        if !alive {
            return Err("cancelled".into());
        }
    }
    file.flush().await.map_err(|e| format!("spool: {e}"))?;
    complete(got, total)?;
    Ok((got, super::volumes::to_hex(&hasher.finalize())))
}

/// A body that stopped short of its declared length is a truncated download,
/// however cleanly the connection closed.
fn complete(got: u64, total: Option<u64>) -> Result<(), String> {
    match total {
        Some(t) if got != t => Err(format!("download ended at {got} of {t} bytes")),
        _ if got == 0 => Err("the server sent an empty file".into()),
        _ => Ok(()),
    }
}

async fn finish(
    pools: &Pools,
    id: &str,
    spool: &std::path::Path,
    size: u64,
    digest: &str,
    expect: Option<Expect>,
) -> Result<String, String> {
    update(id, |j| j.phase = Phase::Verifying);
    match expect {
        Some(Expect::Sha256(want)) if want != digest => {
            return Err(format!("sha256 mismatch: expected {want}, got {digest}"));
        }
        _ => {}
    }
    if !update(id, |j| j.phase = Phase::Writing) {
        return Err("cancelled".into());
    }
    let (pool, name) = {
        let map = jobs().lock().expect("fetch jobs poisoned");
        let j = map.get(id).ok_or("job vanished")?;
        (j.pool.clone(), j.name.clone())
    };
    // The bytes are already complete; the write is a copy into a temp file in
    // the pool root, hashed again against the download and renamed into place
    // only when they agree. A mismatch leaves no volume behind.
    let pools = pools.clone();
    let spool = spool.to_path_buf();
    let want = digest.to_string();
    let (written, placed) = tokio::task::spawn_blocking(move || {
        super::volumes::place_spooled(&pools, &pool, &name, &spool, false, Some(&want))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if placed != digest {
        return Err(format!(
            "written volume hash {placed} differs from the download {digest}"
        ));
    }
    if written != size {
        return Err(format!("wrote {written} bytes but downloaded {size}"));
    }
    Ok(placed)
}

async fn list() -> Json<Value> {
    let now = now_ms();
    let mut map = jobs().lock().expect("fetch jobs poisoned");
    map.retain(|_, j| j.finished_ms == 0 || now.saturating_sub(j.finished_ms) < FINISHED_TTL_MS);
    let mut all: Vec<&Job> = map.values().collect();
    all.sort_by_key(|j| j.started_ms);
    Json(json!(all))
}

/// Cancel a running fetch, or dismiss a finished one.
async fn cancel(Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let mut map = jobs().lock().expect("fetch jobs poisoned");
    let Some(j) = map.get_mut(&id) else {
        return Err(ApiError::not_found("no such fetch"));
    };
    if is_live(j.phase) {
        // The task notices at its next chunk and cleans up its spool file.
        j.cancel = true;
        Ok(Json(json!({"id": id, "cancelled": true})))
    } else {
        map.remove(&id);
        Ok(Json(json!({"id": id, "dismissed": true})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str, name: Option<&str>, sha: Option<&str>) -> StartReq {
        StartReq {
            url: url.into(),
            name: name.map(Into::into),
            sha256: sha.map(Into::into),
        }
    }

    #[test]
    fn the_name_defaults_to_the_urls_file_name() {
        let (_, n, _) = check(&req(
            "https://x.org/iso/alpine-virt-3.20.iso?dl=1",
            None,
            None,
        ))
        .unwrap();
        assert_eq!(n, "alpine-virt-3.20.iso");
        let (_, n, _) = check(&req("https://x.org/a%20b.iso", None, None)).unwrap();
        assert_eq!(n, "a b.iso", "percent-escapes are decoded");
        let (_, n, _) = check(&req("https://x.org/a.iso", Some("  mine.iso "), None)).unwrap();
        assert_eq!(n, "mine.iso");
    }

    #[test]
    fn bad_input_is_refused_before_anything_is_fetched() {
        assert!(
            check(&req("ftp://x.org/a.iso", None, None)).is_err(),
            "scheme"
        );
        assert!(
            check(&req("file:///etc/shadow", None, None)).is_err(),
            "local file"
        );
        assert!(check(&req("not a url", None, None)).is_err());
        assert!(
            check(&req("https://x.org/", None, None)).is_err(),
            "no file name"
        );
        assert!(
            check(&req("https://x.org/a.iso", Some("../etc/x"), None)).is_err(),
            "traversal"
        );
        assert!(
            check(&req("https://x.org/%2e%2e", None, None)).is_err(),
            "encoded .."
        );
        assert!(
            check(&req("https://x.org/a.iso", None, Some("abc"))).is_err(),
            "short digest"
        );
        let (_, _, s) = check(&req("https://x.org/a.iso", None, Some(&"AB".repeat(32)))).unwrap();
        assert_eq!(
            s.as_deref(),
            Some(&*"ab".repeat(32)),
            "digest normalised to lowercase"
        );
    }

    #[test]
    fn a_percent_before_a_multibyte_character_does_not_panic() {
        assert_eq!(percent_decode("a%é.iso"), "a%é.iso");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("x%4"), "x%4");
    }

    fn pool_fixture(tag: &str) -> (Pools, std::path::PathBuf, std::path::PathBuf) {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-pools")
            .join(format!("fetch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("images");
        std::fs::create_dir_all(&root).unwrap();
        let pools = Pools::load(dir.join("state/pools.json")).unwrap();
        let key = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";
        {
            let mut s = pools.lock();
            s.pools.insert(
                key.into(),
                crate::pools::Pool {
                    title: String::new(),
                    path: root.clone(),
                    autostart: true,
                    active: true,
                },
            );
            pools.save(&s).unwrap();
        }
        (pools, root, dir)
    }

    /// A fetch refuses before downloading: an existing volume (409) and a root
    /// that became a symlink (409), both without opening a connection.
    #[tokio::test]
    async fn a_fetch_refuses_an_existing_volume_or_a_hijacked_root() {
        use axum::http::StatusCode;
        const KEY: &str = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";
        let (pools, root, dir) = pool_fixture("refuse");
        let url = || reqwest::Url::parse("https://example.invalid/x.iso").unwrap();

        std::fs::write(root.join("x.iso"), b"old").unwrap();
        let e = begin(pools.clone(), KEY.into(), url(), "x.iso".into(), None)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(e.message.contains("already exists"), "{}", e.message);

        let e = begin(pools.clone(), KEY.into(), url(), "new.iso".into(), None).await;
        // The URL is unreachable, but begin accepted it and spawned a download;
        // refuse instead of starting one by making the pool inactive.
        assert!(e.is_ok(), "a free name passes the up-front checks");

        pools.lock().pools.get_mut(KEY).unwrap().active = false;
        let e = begin(pools.clone(), KEY.into(), url(), "other.iso".into(), None)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(e.message.contains("not active"), "{}", e.message);
        pools.lock().pools.get_mut(KEY).unwrap().active = true;

        // A root replaced by a symlink is refused with the re-point reason.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::rename(&root, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        let e = begin(pools, KEY.into(), url(), "third.iso".into(), None)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(e.message.contains("re-point the pool"), "{}", e.message);
        assert!(!elsewhere.join("third.iso").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_short_body_is_a_failure_even_on_a_clean_close() {
        assert!(complete(100, Some(100)).is_ok());
        assert!(complete(99, Some(100)).is_err());
        assert!(complete(101, Some(100)).is_err());
        assert!(
            complete(50, None).is_ok(),
            "no declared length: take what came"
        );
        assert!(complete(0, None).is_err(), "an empty file is not an ISO");
    }
}
