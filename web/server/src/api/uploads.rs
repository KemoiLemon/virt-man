// Two-phase ISO upload.
//
// The browser used to stream the file straight through the proxy to the
// daemon, which left no way to tell a completed transfer from a corrupted one:
// the daemon answers 200 for both. The file is now staged on this server
// first, so the bytes we are about to send are on local disk. After the daemon
// has them, both sides are hashed and compared, and only a match removes the
// staged copy.
//
// Progress is reported in two stages because they fail differently: the
// browser->server hop can be retried by re-picking the file, while the
// server->daemon hop continues without the browser and is worth surviving a
// page refresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::ApiError;
use crate::auth;
use crate::daemon;
use crate::state::AppState;
use crate::store::User;

/// How far along a staged upload is. Serialised snake_case so the SPA can
/// switch on it without a lookup table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Body received; the daemon push has not started.
    Staged,
    /// Streaming to the daemon.
    Uploading,
    /// Transferred; hashing both sides.
    Verifying,
    /// Hashes match and the staged copy is gone.
    Done,
    /// Something failed. The staged copy is kept so it can be retried.
    Failed,
}

#[derive(Debug)]
pub struct UploadJob {
    pub id: String,
    pub conn: String,
    pub pool: String,
    pub name: String,
    pub size: u64,
    pub sent: u64,
    pub phase: Phase,
    /// SHA-256 of the staged bytes, filled in during verification.
    pub sha256: Option<String>,
    pub error: Option<String>,
    pub at_ms: u64,
    /// Who started it. The transfer outlives the browser request, so the
    /// activity entry is written from the background task.
    pub user_id: String,
    pub username: String,
}

pub type JobRef = Arc<Mutex<UploadJob>>;

/// Active uploads, keyed by id. Deliberately in memory: a restart drops
/// in-flight jobs, and the startup sweep then removes their staged files.
pub type Uploads = Arc<RwLock<HashMap<String, JobRef>>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn view(j: &UploadJob) -> Value {
    json!({
        "id": j.id,
        "conn": j.conn,
        "pool": j.pool,
        "name": j.name,
        "size": j.size,
        "sent": j.sent,
        "phase": j.phase,
        "sha256": j.sha256,
        "error": j.error,
        "at_ms": j.at_ms,
    })
}

/// Directory holding staged bodies.
pub fn dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("uploads")
}

/// Staged path for an id. Ids are generated server-side as hex, and any id
/// arriving from a client is looked up in the map before it is used as a path,
/// so this cannot be reached with a traversing value.
fn staged_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    dir(data_dir).join(format!("{id}.part"))
}

/// Delete staged bodies left by a previous run. Jobs live only in memory, so
/// anything on disk at startup is orphaned by definition and would otherwise
/// accumulate multi-GB files forever.
pub fn sweep(data_dir: &std::path::Path) {
    let d = dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&d) else {
        return;
    };
    for e in entries.flatten() {
        let _ = std::fs::remove_file(e.path());
    }
}

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/uploads", get(list).post(stage))
        .route("/uploads/{id}", get(one).delete(discard))
        .route("/uploads/{id}/push", post(push))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

/// Stage a request body on this server.
///
/// `?conn=&pool=&name=` describe where it will eventually go. The body is
/// written to disk as it arrives, so a multi-GB ISO never sits in memory.
async fn stage(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "storage", true)?;

    let conn = q.get("conn").cloned().unwrap_or_default();
    let pool = q.get("pool").cloned().unwrap_or_default();
    let name = q.get("name").cloned().unwrap_or_default();
    if conn.is_empty() || pool.is_empty() || name.is_empty() {
        return Err(ApiError::bad_request("conn, pool and name are required"));
    }
    // The target has to be valid *now*: finding out after a multi-GB upload
    // that the connection or the name was never usable wastes the user's time.
    let conn_rec = state
        .read()
        .connection(&conn)
        .cloned()
        .ok_or_else(|| ApiError::not_found("no such connection"))?;
    let addr = daemon::parse_url(&conn_rec.url).map_err(ApiError::bad_request)?;
    let client = daemon::http_client(&conn_rec).map_err(ApiError::internal)?;
    // The daemon refuses to overwrite a volume, so an existing name would fail
    // at the end of the transfer with a raw libvirt error. Refuse it up front.
    if volume_exists(&client, &addr.base, &conn_rec.token, &pool, &name)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict(format!(
            "a volume named '{name}' already exists in pool '{pool}' — \
             delete it first, or upload under a different name"
        )));
    }

    let id = crate::store::random_hex(8);
    let path = staged_path(&state.data_dir, &id);
    tokio::fs::create_dir_all(dir(&state.data_dir))
        .await
        .map_err(ApiError::internal)?;

    let size = spool(body, &path).await?;
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::bad_request("empty body"));
    }

    let job = UploadJob {
        id: id.clone(),
        conn,
        pool,
        name,
        size,
        sent: 0,
        phase: Phase::Staged,
        sha256: None,
        error: None,
        at_ms: now_ms(),
        user_id: user.id.clone(),
        username: user.username.clone(),
    };
    let view_json = view(&job);
    state
        .uploads
        .write()
        .expect("uploads lock poisoned")
        .insert(id, Arc::new(Mutex::new(job)));

    Ok(Json(view_json))
}

/// Stream a body to `path`, removing the file if it fails part-way so an
/// interrupted upload cannot leave a multi-GB file behind. The guarantee lives
/// here rather than at the call site, so no future caller can forget it.
async fn spool(body: Body, path: &std::path::Path) -> Result<u64, ApiError> {
    match spool_inner(body, path).await {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = tokio::fs::remove_file(path).await;
            Err(e)
        }
    }
}

async fn spool_inner(body: Body, path: &std::path::Path) -> Result<u64, ApiError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(ApiError::internal)?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk.map_err(ApiError::bad_request)?;
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
        total += chunk.len() as u64;
    }
    file.flush().await.map_err(ApiError::internal)?;
    Ok(total)
}

fn lookup(state: &AppState, id: &str) -> Result<JobRef, ApiError> {
    state
        .uploads
        .read()
        .expect("uploads lock poisoned")
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("no such upload"))
}

async fn one(
    State(state): State<AppState>,
    axum::extract::Extension(_user): axum::extract::Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = lookup(&state, &id)?;
    let j = job.lock().expect("job lock poisoned");
    Ok(Json(view(&j)))
}

async fn list(
    State(state): State<AppState>,
    axum::extract::Extension(_user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    let jobs: Vec<JobRef> = state
        .uploads
        .read()
        .expect("uploads lock poisoned")
        .values()
        .cloned()
        .collect();
    let mut out: Vec<Value> = jobs
        .iter()
        .map(|j| view(&j.lock().expect("job lock poisoned")))
        .collect();
    // Oldest first, so the UI shows them in the order they were started.
    out.sort_by_key(|v| v.get("at_ms").and_then(Value::as_u64).unwrap_or(0));
    Ok(Json(json!(out)))
}

/// Drop a staged upload and its file.
async fn discard(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "storage", true)?;
    let job = lookup(&state, &id)?;
    {
        let j = job.lock().expect("job lock poisoned");
        if j.phase == Phase::Uploading || j.phase == Phase::Verifying {
            return Err(ApiError::conflict("upload is still running"));
        }
    }
    tokio::fs::remove_file(staged_path(&state.data_dir, &id))
        .await
        .ok();
    state
        .uploads
        .write()
        .expect("uploads lock poisoned")
        .remove(&id);
    Ok(Json(json!({"id": id, "discarded": true})))
}

/// Start pushing a staged file to the daemon. Returns immediately; the client
/// polls `GET /uploads/{id}` for progress.
async fn push(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "storage", true)?;
    let job = lookup(&state, &id)?;
    {
        let mut j = job.lock().expect("job lock poisoned");
        if j.phase == Phase::Uploading || j.phase == Phase::Verifying {
            return Err(ApiError::conflict("upload already running"));
        }
        if j.phase == Phase::Done {
            return Err(ApiError::conflict("upload already completed"));
        }
        // Clear a previous failure so the UI shows the retry honestly.
        j.phase = Phase::Uploading;
        j.sent = 0;
        j.error = None;
    }

    let bg = state.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        let result = transfer(&bg, &job_id, job.clone()).await;
        let (label, message) = match &result {
            Ok(hash) => (
                format!("Upload {}", job.lock().expect("job lock poisoned").name),
                format!("verified sha256 {}", &hash[..hash.len().min(16)]),
            ),
            Err(e) => {
                let mut j = job.lock().expect("job lock poisoned");
                j.phase = Phase::Failed;
                j.error = Some(e.clone());
                (format!("Upload {}", j.name), e.clone())
            }
        };
        // Recorded here rather than in the browser: the browser may be gone by
        // now, and a silently failed transfer is exactly what must not vanish.
        let (user_id, username) = {
            let j = job.lock().expect("job lock poisoned");
            (j.user_id.clone(), j.username.clone())
        };
        let status = if result.is_ok() { "done" } else { "failed" };
        if let Err(e) = super::activity::append(&bg, &user_id, &username, &label, status, &message)
        {
            tracing::warn!("could not record upload activity: {e}");
        }
    });

    Ok(Json(json!({"id": id, "phase": Phase::Uploading})))
}

/// Stream the staged file to the daemon, then verify the daemon has the same
/// bytes and remove the staged copy only if it does. Returns the verified hash.
async fn transfer(state: &AppState, id: &str, job: JobRef) -> Result<String, String> {
    let (conn_id, pool, name, size) = {
        let j = job.lock().expect("job lock poisoned");
        (j.conn.clone(), j.pool.clone(), j.name.clone(), j.size)
    };
    let path = staged_path(&state.data_dir, id);

    let conn = {
        let store = state.read();
        store
            .connection(&conn_id)
            .cloned()
            .ok_or("connection disappeared")?
    };
    let addr = daemon::parse_url(&conn.url)?;
    let client = daemon::http_client(&conn)?;

    // --- push the bytes -----------------------------------------------------
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("open staged file: {e}"))?;
    // Count bytes as they leave, so `sent` is real progress rather than a
    // guess. ReaderStream keeps the transfer chunked: a multi-GB ISO is never
    // held in memory.
    let progress = job.clone();
    let stream = tokio_util::io::ReaderStream::new(file).inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            if let Ok(mut j) = progress.lock() {
                j.sent += bytes.len() as u64;
            }
        }
    });

    let url = format!(
        "{}/pools/{}/volumes/{}/upload?name={}",
        addr.base,
        urlencode(&pool),
        urlencode(&name),
        urlencode(&name)
    );
    let resp = client
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", conn.token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size.to_string())
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|e| format!("daemon connection: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("daemon refused the upload: HTTP {status} {body}"));
    }

    // --- verify ------------------------------------------------------------
    {
        let mut j = job.lock().expect("job lock poisoned");
        j.phase = Phase::Verifying;
    }
    let path_for_hash = path.clone();
    let local = tokio::task::spawn_blocking(move || sha256_file(&path_for_hash))
        .await
        .map_err(|e| format!("hash task: {e}"))??;

    let remote = daemon_hash(&client, &addr.base, &conn.token, &pool, &name).await?;
    verify(&local, &remote)?;

    // --- clean up ----------------------------------------------------------
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| format!("remove staged file: {e}"))?;
    let mut j = job.lock().expect("job lock poisoned");
    j.phase = Phase::Done;
    j.sha256 = Some(local.clone());
    Ok(local)
}

/// Whether the bytes the daemon stored are the bytes we sent.
///
/// Split out as a pure function because this is the decision the whole staging
/// design exists to make, and it must not be reachable only through a
/// multi-gigabyte round trip.
fn verify(local: &str, remote: &str) -> Result<(), String> {
    if local == remote {
        return Ok(());
    }
    Err(format!(
        "hash mismatch — the daemon stored {remote}, the staged file is {local}; \
         the staged copy was kept"
    ))
}

/// Is a volume of this name already in the pool?
///
async fn volume_exists(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    pool: &str,
    name: &str,
) -> Result<bool, String> {
    let url = format!(
        "{}/pools/{}/volumes/{}",
        base,
        urlencode(pool),
        urlencode(name)
    );
    let resp = client
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("daemon lookup: {e}"))?;
    Ok(resp.status().is_success())
}

/// SHA-256 of a file, streamed so a multi-GB ISO never enters memory.
fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("open for hash: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read for hash: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Ask the daemon for the stored volume's hash.
async fn daemon_hash(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    pool: &str,
    name: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/pools/{}/volumes/{}/hash",
        base,
        urlencode(pool),
        urlencode(name)
    );
    let resp = client
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("daemon hash request: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("daemon hash failed: HTTP {status} {v}"));
    }
    v.get("sha256")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("daemon hash response had no sha256: {v}"))
}

/// Percent-encode a path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision the whole staging design exists to make. Tested directly
    /// because it is otherwise only reachable after a multi-gigabyte round
    /// trip, and a wrong answer either loses the user's file or hides a bad
    /// transfer.
    #[test]
    fn matching_hashes_verify() {
        let h = "7d67c7f89862c3379b178f5efa34a0c1c4bb9beaa289cfada29037a387882d6b";
        assert!(verify(h, h).is_ok());
    }

    #[test]
    fn a_different_hash_is_a_failure_that_mentions_both_sides() {
        let err = verify("aaaa", "bbbb").expect_err("differing hashes must not verify");
        // Both digests must appear: the message is the only clue to which side
        // went wrong, and it says the staged copy was kept.
        assert!(err.contains("aaaa"), "local hash missing: {err}");
        assert!(err.contains("bbbb"), "remote hash missing: {err}");
        assert!(
            err.contains("kept"),
            "must say the staged copy was kept: {err}"
        );
    }

    /// A truncated transfer is the exact failure the daemon used to report as
    /// success; it must never verify.
    #[test]
    fn a_truncated_hash_never_matches() {
        let full = "7d67c7f89862c3379b178f5efa34a0c1c4bb9beaa289cfada29037a387882d6b";
        assert!(verify(full, &full[..32]).is_err());
        assert!(verify(full, "").is_err());
    }

    #[test]
    fn urlencode_escapes_path_separators() {
        // A volume name is one path segment; an unescaped slash would let a
        // name reach a different daemon route entirely.
        assert_eq!(urlencode("plain.iso"), "plain.iso");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("with space.iso"), "with%20space.iso");
        assert_eq!(urlencode("../etc"), "..%2Fetc");
    }

    #[test]
    fn staged_paths_stay_inside_the_uploads_dir() {
        let dir = std::path::Path::new("/data");
        let p = staged_path(dir, "abc123");
        assert_eq!(p, std::path::Path::new("/data/uploads/abc123.part"));
    }

    /// The hash of the staged bytes must equal the hash of the same bytes
    /// written through `spool`, since that equality is the whole verification.
    #[tokio::test]
    async fn spool_writes_exactly_the_body_it_hashes() {
        let dir = std::env::temp_dir().join("virtman-upload-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.part");
        let data = vec![9u8; 300 * 1024];
        let body = Body::from(data.clone());
        // Not `.unwrap()`: ApiError is intentionally not Debug.
        let n = match spool(body, &path).await {
            Ok(n) => n,
            Err(_) => panic!("spool should succeed"),
        };
        assert_eq!(n, data.len() as u64);
        let got = sha256_file(&path).unwrap();
        let mut h = Sha256::new();
        h.update(&data);
        let want: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want);
        let _ = std::fs::remove_file(&path);
    }

    /// A failed body must not leave the staged file behind.
    #[tokio::test]
    async fn spool_removes_the_file_when_the_body_fails() {
        let dir = std::env::temp_dir().join("virtman-upload-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.part");
        let body = Body::from_stream(futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("connection reset")),
        ]));
        assert!(spool(body, &path).await.is_err());
        assert!(!path.exists(), "staged file must be cleaned up");
    }

    /// The sweep only touches the uploads directory.
    #[test]
    fn sweep_removes_orphaned_staged_files() {
        let dir = std::env::temp_dir().join("virtman-sweep-test");
        let up = dir.join("uploads");
        std::fs::create_dir_all(&up).unwrap();
        let keep = dir.join("store.json");
        std::fs::write(&keep, b"x").unwrap();
        let orphan = up.join("dead.part");
        std::fs::write(&orphan, b"junk").unwrap();
        sweep(&dir);
        assert!(!orphan.exists(), "orphaned staged file removed");
        assert!(keep.exists(), "unrelated files left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
