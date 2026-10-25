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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    /// When the job reached a terminal phase, or 0 while it has not. Used to
    /// expire finished transfers (see [`expired`]); `at_ms` is when the job was
    /// staged, which for a long upload is a poor guide to when it finished.
    pub finished_ms: u64,
    /// When the daemon leg started, or 0 before it does.
    pub started_ms: u64,
    /// Average bytes/second of the daemon leg, recomputed as bytes leave.
    /// Average rather than instantaneous: a per-chunk rate jitters far too much
    /// to read, and this is what "how fast is this going" actually means.
    pub speed_bps: f64,    /// Who started it. The transfer outlives the browser request, so the
    /// activity entry is written from the background task.
    pub user_id: String,
    pub username: String,
    /// The daemon-leg task, so a cancel can drop its request mid-body.
    pub abort: Option<tokio::task::AbortHandle>,
}

/// The error a cancelled daemon send is left with; the UI matches it to say
/// "Cancelled" rather than "Failed".
const CANCELLED: &str = "Cancelled";

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
        "speed_bps": j.speed_bps,    })
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

/// Sidecar describing a staged upload.
///
/// Its presence is also the **completeness marker**: it is written only once the
/// body has arrived in full. That distinction matters because the hash check at
/// the end of a transfer proves the bytes *travelled* intact, not that the
/// staged file was ever complete — pushing a half-written file would send
/// partial bytes, the daemon would store exactly those, and the two hashes
/// would agree. So a `.part` with no sidecar is never resumable.
#[derive(Serialize, Deserialize)]
struct StagedMeta {
    id: String,
    conn: String,
    pool: String,
    name: String,
    size: u64,
    user_id: String,
    username: String,
    at_ms: u64,
}

fn staged_meta_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    dir(data_dir).join(format!("{id}.json"))
}

/// Remove both files belonging to a staged upload.
fn remove_staged(data_dir: &std::path::Path, id: &str) {
    let _ = std::fs::remove_file(staged_path(data_dir, id));
    let _ = std::fs::remove_file(staged_meta_path(data_dir, id));
}

/// Decide what to do with whatever is in the staging directory after a restart,
/// without touching the filesystem: `(jobs to resume, files to delete)`.
///
/// Pure so every branch is testable, including the one that must never resume a
/// partial transfer.
fn plan_restore(d: &std::path::Path) -> (Vec<UploadJob>, Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(d) else {
        return (Vec::new(), Vec::new());
    };

    // Ids mentioned by either file, so a body and its sidecar are considered
    // together rather than as two independent files.
    let mut ids: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(id) = name
            .strip_suffix(".part")
            .or_else(|| name.strip_suffix(".json"))
        {
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }
    }

    let mut jobs = Vec::new();
    let mut delete = Vec::new();
    for id in ids {
        let body = d.join(format!("{id}.part"));
        match job_from_files(&body, &d.join(format!("{id}.json")), &id) {
            Some(job) => jobs.push(job),
            None => {
                delete.push(body);
                delete.push(d.join(format!("{id}.json")));
            }
        }
    }
    (jobs, delete)
}

/// Rebuild a job from its staged files, or `None` if it cannot be trusted.
fn job_from_files(body: &std::path::Path, meta: &std::path::Path, id: &str) -> Option<UploadJob> {
    let raw = std::fs::read_to_string(meta).ok()?;
    let m: StagedMeta = serde_json::from_str(&raw).ok()?;
    // The sidecar must describe the file it sits next to. Ids are generated
    // server-side, so a mismatch means the directory was edited by hand.
    if m.id != id {
        return None;
    }
    // A body that is not the size we recorded was truncated after staging, so
    // resuming it would push a short file that still hashes consistently.
    let on_disk = std::fs::metadata(body).ok()?.len();
    if on_disk != m.size {
        return None;
    }
    let now = now_ms();
    Some(UploadJob {
        id: m.id,
        conn: m.conn,
        pool: m.pool,
        name: m.name,
        size: m.size,
        sent: 0,
        // Resumed jobs come back ready to push, never mid-flight: the old
        // attempt is gone and its progress with it.
        phase: Phase::Staged,
        sha256: None,
        error: None,
        at_ms: if m.at_ms == 0 { now } else { m.at_ms },
        // Not finished: it has not run yet in this process, so it must never be
        // treated as expired and silently dropped.
        finished_ms: 0,
        started_ms: 0,
        speed_bps: 0.0,        user_id: m.user_id,
        username: m.username,
        abort: None,
    })
}

/// Recover fully-staged uploads across a restart and delete the rest.
///
/// Jobs live in memory, so a restart drops them; without this the startup sweep
/// would also destroy their staged bodies, turning a survivable interruption
/// into a multi-GB re-upload from the browser. Anything incomplete is removed,
/// since it can never be resumed safely.
pub fn restore(state: &AppState) {
    let (jobs, delete) = plan_restore(&dir(&state.data_dir));
    for p in delete {
        let _ = std::fs::remove_file(p);
    }
    if jobs.is_empty() {
        return;
    }
    let mut map = state.uploads.write().expect("uploads lock poisoned");
    for job in jobs {
        let name = job.name.clone();
        let size = job.size;
        map.insert(job.id.clone(), Arc::new(Mutex::new(job)));
        tracing::info!("resumed staged upload {name} ({size} bytes); push to send it");
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
    let conn = q.get("conn").cloned().unwrap_or_default();
    let pool = q.get("pool").cloned().unwrap_or_default();
    let name = q.get("name").cloned().unwrap_or_default();
    if conn.is_empty() || pool.is_empty() || name.is_empty() {
        return Err(ApiError::bad_request("conn, pool and name are required"));
    }
    super::enforce(
        &state,
        &user,
        &crate::perm::pool_path(&conn, &pool),
        "Datastore.AllocateSpace",
    )?;
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
    // The size is known only once the body is here; checked before anything
    // reaches the daemon. Administrators have no quota. An ISO is shared media
    // and belongs to nobody, so it costs no one storage and is not held to a
    // quota at all.
    if !state.read().is_admin(&user) && !crate::guard::is_iso(&name) {
        let quota = crate::guard::check_quota(
            &state,
            &conn_rec,
            &user,
            crate::perm::Usage {
                storage_bytes: size,
                ..Default::default()
            },
        )
        .await;
        if let Err(e) = quota {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(e);
        }
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
        finished_ms: 0,
        started_ms: 0,
        speed_bps: 0.0,        user_id: user.id.clone(),
        username: user.username.clone(),
        abort: None,
    };
    let view_json = view(&job);
    // Written *after* the body is complete, which is what makes it a
    // completeness marker: a restart finds both files only if the whole body
    // arrived, and drops a `.part` with no sidecar rather than resuming it.
    let meta = StagedMeta {
        id: job.id.clone(),
        conn: job.conn.clone(),
        pool: job.pool.clone(),
        name: job.name.clone(),
        size: job.size,
        user_id: job.user_id.clone(),
        username: job.username.clone(),
        at_ms: job.at_ms,
    };
    if let Err(e) = tokio::fs::write(
        staged_meta_path(&state.data_dir, &id),
        serde_json::to_vec(&meta).unwrap_or_default(),
    )
    .await
    {
        // Without the sidecar the upload would not survive a restart, so treat
        // it as a failed stage rather than silently accepting a job that could
        // be dropped later.
        remove_staged(&state.data_dir, &id);
        return Err(ApiError::internal(format!("record staged upload: {e}")));
    }
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

/// A staged upload is its uploader's to send or discard, or an admin's.
fn own_job(state: &AppState, user: &User, job: &JobRef) -> Result<(), ApiError> {
    let owner = job.lock().expect("job lock poisoned").user_id.clone();
    if owner == user.id || state.read().is_admin(user) {
        Ok(())
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "this upload belongs to another user",
        ))
    }
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
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = lookup(&state, &id)?;
    own_job(&state, &user, &job)?;
    let j = job.lock().expect("job lock poisoned");
    Ok(Json(view(&j)))
}

async fn list(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    let admin = state.read().is_admin(&user);
    // Forget finished transfers that have been on screen long enough. Done on
    // this path rather than a timer: the pool page polls it every second while
    // it is open, which is exactly when the clutter would be visible, and a
    // server nobody is watching has no one to show it to.
    let now = now_ms();
    {
        let mut map = state.uploads.write().expect("uploads lock poisoned");
        prune(&mut map, now);
    }
    let jobs: Vec<JobRef> = state
        .uploads
        .read()
        .expect("uploads lock poisoned")
        .values()
        .filter(|j| admin || j.lock().expect("job lock poisoned").user_id == user.id)
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
    let job = lookup(&state, &id)?;
    own_job(&state, &user, &job)?;
    {
        let mut j = job.lock().expect("job lock poisoned");
        if j.phase == Phase::Verifying {
            return Err(ApiError::conflict("upload is being verified"));
        }
        // Cancelling the daemon leg: dropping the task drops its request, which
        // closes the connection. The staged copy is kept, so this is a Failed
        // job the user can dismiss; the task cannot record it, being aborted.
        if j.phase == Phase::Uploading {
            if let Some(h) = j.abort.take() {
                h.abort();
            }
            j.phase = Phase::Failed;
            j.error = Some(CANCELLED.into());
            j.finished_ms = now_ms();
            let (uid, uname, label) = (j.user_id.clone(), j.username.clone(), format!("Upload {}", j.name));
            drop(j);
            if let Err(e) = super::activity::append(&state, &uid, &uname, &label, "failed", CANCELLED) {
                tracing::warn!("could not record upload activity: {e}");
            }
            return Ok(Json(json!({"id": id, "cancelled": true})));
        }
    }
    // Both files go, including the sidecar: leaving it behind would make the
    // next startup treat the id as resume-able when its body is gone.
    remove_staged(&state.data_dir, &id);
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
    let job = lookup(&state, &id)?;
    own_job(&state, &user, &job)?;
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
        j.speed_bps = 0.0;        j.error = None;
        j.started_ms = now_ms();
    }

    let bg = state.clone();
    let job_id = id.clone();
    let tracked = job.clone();
    let handle = tokio::spawn(async move {
        let result = transfer(&bg, &job_id, job.clone()).await;
        let (label, message) = match &result {
            Ok(()) => {
                let j = job.lock().expect("job lock poisoned");
                (
                    format!("Upload {}", j.name),
                    format!("stored — {} to daemon", fmt_bps(j.speed_bps)),
                )
            }
            Err(e) => {
                let mut j = job.lock().expect("job lock poisoned");
                j.phase = Phase::Failed;
                j.error = Some(e.clone());
                j.finished_ms = now_ms();
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
    tracked.lock().expect("job lock poisoned").abort = Some(handle.abort_handle());

    Ok(Json(json!({"id": id, "phase": Phase::Uploading})))
}

/// Stream the staged file to the daemon, and remove the staged copy once the
/// daemon reports it stored every byte.
async fn transfer(state: &AppState, id: &str, job: JobRef) -> Result<(), String> {
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
                // Average rate over the whole push. Computed here rather than
                // in the UI so the figure is a measurement, not a guess made
                // from however often the browser happened to poll.
                let elapsed = now_ms().saturating_sub(j.started_ms) as f64 / 1000.0;
                if elapsed > 0.0 {
                    j.speed_bps = j.sent as f64 / elapsed;
                }
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
    // No hashing: the daemon writes straight into the pool and renames when the
    // body is complete, so the only check is that it stored as many bytes as
    // were staged. A short body fails here and the staged copy is kept.
    let stored = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("uploaded").and_then(Value::as_u64));
    if stored != Some(size) {
        return Err(format!(
            "the daemon stored {} of {size} bytes; the staged copy was kept",
            stored.map_or("an unknown number".to_string(), |n| n.to_string())
        ));
    }

    // --- clean up ----------------------------------------------------------
    // Both files, so a completed transfer leaves nothing for the next startup
    // to consider resuming.
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| format!("remove staged file: {e}"))?;
    let _ = tokio::fs::remove_file(staged_meta_path(&state.data_dir, id)).await;
    let mut j = job.lock().expect("job lock poisoned");
    j.phase = Phase::Done;
    j.finished_ms = now_ms();
    // ISO media is shared: it belongs to nobody, so it counts against no one's
    // storage and no ownership record is written. `owner_of` then falls back to
    // the first administrator, exactly as for an ISO copied into the pool by
    // hand, and a non-admin cannot delete one. Anything else is the uploader's:
    // they may delete it, and it counts against their storage. Not `blank`:
    // its contents are theirs, so it is only ever attached as raw (see
    // `guard`).
    let ownerless = crate::guard::is_iso(&j.name);
    let record = (!ownerless).then(|| crate::perm::Resource {
        kind: crate::perm::Kind::Volume,
        conn: j.conn.clone(),
        pool: j.pool.clone(),
        name: j.name.clone(),
        owner: j.user_id.clone(),
        suspended: false,
        path: String::new(),
        capacity: j.size,
        blank: false,
        created_ms: now_ms(),
    });
    drop(j);
    if let Some(record) = record {
        let mut store = state.write();
        store.resources.push(record);
        if let Err(e) = store.save(&state.data_dir) {
            tracing::warn!("could not record upload owner: {e}");
        }
    }
    Ok(())
}

/// Human-readable transfer rate. Kept in step with the SPA's `fmtRate` (KiB/s
/// up to 1024, then MiB/s) so a speed recorded in the activity log reads the
/// same way as the same speed shown live in the UI.
fn fmt_bps(bps: f64) -> String {
    const KIB: f64 = 1024.0;
    if !bps.is_finite() || bps <= 0.0 {
        return "0 KiB/s".to_string();
    }
    let kib = bps / KIB;
    if kib < KIB {
        return if kib >= 10.0 {
            format!("{kib:.0} KiB/s")
        } else {
            format!("{kib:.1} KiB/s")
        };
    }
    format!("{:.1} MiB/s", kib / KIB)
}

/// How long a finished transfer stays listed. Long enough to read the result of
/// a multi-GB upload, short enough that the pool page shows current work rather
/// than a growing wall of old transfers.
const FINISHED_TTL_MS: u64 = 15 * 60 * 1000;

/// Whether a finished job is old enough to forget.
///
/// The activity log is the durable record of what happened, so a completed card
/// is transient progress: dropping it loses nothing that matters, and without
/// this the map and the pool page grow for the life of the process (fourteen
/// jobs piled up during a single session of testing). A job with no finish time
/// is never expired, which keeps this safe for jobs restored from disk — those
/// come back staged, not finished.
fn expired(job: &UploadJob, now: u64) -> bool {
    matches!(job.phase, Phase::Done | Phase::Failed)
        && job.finished_ms > 0
        && now.saturating_sub(job.finished_ms) > FINISHED_TTL_MS
}

/// Forget finished transfers that are past their TTL.
///
/// Separate from the handler so the behaviour can be tested against a real map
/// rather than only through the predicate it uses.
fn prune(map: &mut HashMap<String, JobRef>, now: u64) {
    map.retain(|_, job| !expired(&job.lock().expect("job lock poisoned"), now));
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

    /// A private staging directory for one test, so cases cannot interfere.
    fn staged_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("virtman-restore-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The sidecar body, with `size` as the claimed length.
    fn meta_json(id: &str, size: u64) -> String {
        format!(
            r#"{{"id":"{id}","conn":"c1","pool":"default","name":"some.iso",
               "size":{size},"user_id":"u1","username":"alice","at_ms":123}}"#
        )
    }

    /// A `.part` of `body_len` bytes plus a sidecar claiming `meta_size`, so a
    /// mismatch can be produced on purpose.
    fn write_pair(d: &std::path::Path, id: &str, body_len: usize, meta_size: u64) {
        std::fs::write(d.join(format!("{id}.part")), vec![7u8; body_len]).unwrap();
        std::fs::write(d.join(format!("{id}.json")), meta_json(id, meta_size)).unwrap();
    }

    fn job_in(phase: Phase, finished_ms: u64) -> UploadJob {
        UploadJob {
            id: "j1".into(),
            conn: "c1".into(),
            pool: "default".into(),
            name: "x.iso".into(),
            size: 10,
            sent: 0,
            phase,
            sha256: None,
            error: None,
            at_ms: 1,
            finished_ms,
            started_ms: 0,
            speed_bps: 0.0,            user_id: "u1".into(),
            username: "alice".into(),
            abort: None,
        }
    }

    /// A finished job is kept long enough to read, then forgotten. Without this
    /// the map and the pool page grow for the life of the process.
    #[test]
    fn finished_jobs_expire_after_the_ttl() {
        let now = 1_000_000_000;
        let done = job_in(Phase::Done, now - FINISHED_TTL_MS - 1);
        assert!(expired(&done, now), "an old finished job should expire");
        let fresh = job_in(Phase::Done, now - 5_000);
        assert!(
            !expired(&fresh, now),
            "a just-finished job must stay readable"
        );
    }

    /// A failure is the thing someone most needs to see, so it is held exactly
    /// as long as a success.
    #[test]
    fn failed_jobs_expire_on_the_same_schedule() {
        let now = 1_000_000_000;
        assert!(expired(
            &job_in(Phase::Failed, now - FINISHED_TTL_MS - 1),
            now
        ));
        assert!(!expired(&job_in(Phase::Failed, now - 1_000), now));
    }

    /// Work in progress must never be expired, whatever its timestamps: that
    /// would drop a transfer out of the UI while it is still running.
    #[test]
    fn unfinished_jobs_never_expire() {
        let now = 1_000_000_000;
        let ancient = now - FINISHED_TTL_MS * 100;
        for phase in [Phase::Staged, Phase::Uploading, Phase::Verifying] {
            assert!(
                !expired(&job_in(phase, ancient), now),
                "{phase:?} must not expire"
            );
        }
    }

    /// A zero finish time means "not recorded", which must not be read as
    /// "finished at the epoch" -- that would expire everything immediately.
    #[test]
    fn a_missing_finish_time_never_expires() {
        let now = 1_000_000_000;
        assert!(!expired(&job_in(Phase::Done, 0), now));
    }

    /// A clock that has not advanced past the finish time (or has gone
    /// backwards) must not expire a job; saturating arithmetic keeps this from
    /// wrapping into a huge age.
    #[test]
    fn a_finish_time_in_the_future_does_not_expire() {
        let now = 1_000_000_000;
        assert!(!expired(&job_in(Phase::Done, now + 60_000), now));
        assert!(!expired(&job_in(Phase::Done, now), now));
    }

    /// The prune the handler actually runs, against a real map: it must drop the
    /// stale finished job and keep everything else, including work in progress.
    /// A predicate-only test would not catch the map being locked or filtered
    /// wrongly, which is where this could quietly go wrong.
    #[test]
    fn prune_forgets_only_stale_finished_jobs() {
        let now = 1_000_000_000;
        let mut map: HashMap<String, JobRef> = HashMap::new();
        let mut insert = |name: &str, job: UploadJob| {
            map.insert(name.to_string(), Arc::new(Mutex::new(job)));
        };
        let mut stale = job_in(Phase::Done, now - FINISHED_TTL_MS - 1);
        stale.id = "stale".into();
        insert("stale", stale);
        let mut fresh = job_in(Phase::Done, now - 1_000);
        fresh.id = "fresh".into();
        insert("fresh", fresh);
        let mut running = job_in(Phase::Uploading, 0);
        running.id = "running".into();
        insert("running", running);
        let mut failed = job_in(Phase::Failed, now - FINISHED_TTL_MS - 1);
        failed.id = "failed".into();
        insert("failed", failed);

        prune(&mut map, now);

        let left: Vec<&String> = {
            let mut v: Vec<&String> = map.keys().collect();
            v.sort();
            v
        };
        assert_eq!(left, ["fresh", "running"], "only the stale done job goes");
    }

    /// The thresholds must match the SPA's `fmtRate`, or the same transfer
    /// would read as "1.0 MiB/s" in the log and "1024 KiB/s" in the UI.
    #[test]
    fn fmt_bps_agrees_with_the_ui_unit_boundaries() {
        assert_eq!(fmt_bps(0.0), "0 KiB/s");
        assert_eq!(fmt_bps(-5.0), "0 KiB/s");
        assert_eq!(fmt_bps(f64::NAN), "0 KiB/s");
        assert_eq!(fmt_bps(512.0), "0.5 KiB/s");
        // Just below the boundary stays in KiB...
        assert_eq!(fmt_bps(1023.0 * 1024.0), "1023 KiB/s");
        // ...and exactly 1 MiB/s switches over.
        assert_eq!(fmt_bps(1024.0 * 1024.0), "1.0 MiB/s");
        assert_eq!(fmt_bps(45.6 * 1024.0 * 1024.0), "45.6 MiB/s");
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

    /// The staged file must hold exactly the bytes of the body.
    #[tokio::test]
    async fn spool_writes_exactly_the_body() {
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
        assert_eq!(std::fs::read(&path).unwrap(), data, "spooled bytes are exactly the body");
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

    /// A fully-staged upload survives a restart: the body and its sidecar are
    /// both there, so it comes back ready to push without re-uploading.
    #[test]
    fn a_complete_staged_upload_is_resumed() {
        let d = staged_dir("resume-complete");
        write_pair(&d, "abc123", 8, 8);

        let (jobs, delete) = plan_restore(&d);
        assert_eq!(jobs.len(), 1, "the upload is recovered");
        assert_eq!(jobs[0].name, "some.iso");
        assert_eq!(jobs[0].size, 8);
        // Resumed work comes back ready, never mid-flight: the previous attempt
        // is gone and any partial progress with it.
        assert_eq!(jobs[0].phase, Phase::Staged);
        assert_eq!(jobs[0].sent, 0);
        assert!(delete.is_empty(), "nothing to delete: {delete:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The check that stops a half-written body being resumed. Pushing it would
    /// send partial bytes, the daemon would store exactly those, and the hashes
    /// would agree -- a truncated volume reported as verified.
    #[test]
    fn an_incomplete_staged_upload_is_deleted_not_resumed() {
        let d = staged_dir("resume-partial");
        // Body present, but only part of what the sidecar claims.
        write_pair(&d, "half00", 4, 8);

        let (jobs, delete) = plan_restore(&d);
        assert!(jobs.is_empty(), "a partial body must never be resumed");
        assert_eq!(delete.len(), 2, "both files are removed: {delete:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A body with no sidecar means the process died before the body finished.
    #[test]
    fn a_body_without_a_sidecar_is_deleted() {
        let d = staged_dir("resume-nosidecar");
        std::fs::write(d.join("orphan.part"), b"junk").unwrap();

        let (jobs, delete) = plan_restore(&d);
        assert!(jobs.is_empty());
        assert_eq!(delete.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A sidecar whose body is gone must not resurrect a job that cannot send.
    #[test]
    fn a_sidecar_without_a_body_is_deleted() {
        let d = staged_dir("resume-nobody");
        let id = "metaonly";
        std::fs::write(d.join(format!("{id}.json")), meta_json(id, 8)).unwrap();

        let (jobs, delete) = plan_restore(&d);
        assert!(jobs.is_empty());
        assert_eq!(delete.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A sidecar describing a different id than the file it sits beside means
    /// the directory was edited by hand, so it is not trusted.
    #[test]
    fn a_sidecar_for_another_id_is_rejected() {
        let d = staged_dir("resume-mismatch");
        std::fs::write(d.join("abcdefgh.part"), b"12345678").unwrap();
        std::fs::write(d.join("abcdefgh.json"), meta_json("different", 8)).unwrap();

        let (jobs, _) = plan_restore(&d);
        assert!(jobs.is_empty(), "mismatched sidecar must not be resumed");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Only our own files are touched: an unrelated file in the staging
    /// directory is left alone.
    #[test]
    fn unrelated_files_are_left_alone() {
        let d = staged_dir("resume-other");
        let keep = d.join("notes.txt");
        std::fs::write(&keep, b"x").unwrap();
        write_pair(&d, "keepme00", 4, 4);

        let (jobs, delete) = plan_restore(&d);
        assert_eq!(jobs.len(), 1);
        assert!(delete.is_empty());
        assert!(keep.exists(), "unrelated file removed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
