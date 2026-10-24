//! A container's text console, for the browser.
//!
//! A container has no display: its console is the pty LXC's monitor holds as
//! the container's `/dev/console`. `POST …/console/connect` runs
//! `lxc-console` against it in a pty of our own (`crate::pty`), and the
//! browser long-polls that like the host shell: `read?after=N` parks until
//! there are bytes past offset `N`, `write` sends keystrokes, `resize` carries
//! the terminal's size.
//!
//! One session per container, shared: LXC lets one client hold a console at a
//! time, so a second tab joins the first one's session instead of being
//! refused or stealing it. A session nobody reads closes after `pty::IDLE`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Extension, Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use crate::lxc::{Manager, State};
use crate::pty::{self, Session};

fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static S: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn routes() -> Router {
    Router::new()
        .route("/containers/{name}/console/connect", post(connect))
        .route("/containers/{name}/console/read", get(read))
        .route("/containers/{name}/console/write", post(write))
        .route("/containers/{name}/console/resize", post(resize))
        .route("/containers/{name}/console/disconnect", post(disconnect))
}

fn id_of(cts: &Manager, key: &str) -> Result<String, ApiError> {
    cts.resolve(key)
        .map(|e| e.id().to_string())
        .ok_or_else(|| ApiError::not_found(format!("no container '{key}'")))
}

fn live(id: &str) -> Option<Arc<Session>> {
    let map = sessions().lock().expect("console sessions");
    map.get(id).filter(|s| !s.is_closed()).cloned()
}

#[derive(Deserialize, Default)]
struct ConnectReq {
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Open (or join) the container's console.
async fn connect(
    Extension(cts): Extension<Manager>,
    Path(name): Path<String>,
    body: Option<Json<ConnectReq>>,
) -> Result<Json<Value>, ApiError> {
    let id = id_of(&cts, &name)?;
    if let Some(s) = live(&id) {
        return Ok(Json(json!({"name": id, "offset": s.end(), "joined": true})));
    }
    if cts.state(&id) != State::Running {
        return Err(ApiError::conflict("the container is not running"));
    }
    let req = body.map(|b| b.0).unwrap_or_default();
    let (cols, rows) = (req.cols.unwrap_or(80), req.rows.unwrap_or(24));
    let argv = cts.console_argv(&id);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let gone = id.clone();
    let session = pty::spawn(
        &id,
        &argv,
        &["TERM=xterm-256color"],
        cols,
        rows,
        move || {
            let mut map = sessions().lock().expect("console sessions");
            if map.get(&gone).is_some_and(|s| s.is_closed()) {
                map.remove(&gone);
            }
        },
    )
    .map_err(ApiError::internal)?;
    // Two connects can race; the loser joins the winner's session.
    let mut map = sessions().lock().expect("console sessions");
    if let Some(s) = map.get(&id).filter(|s| !s.is_closed()) {
        session.close();
        return Ok(Json(json!({"name": id, "offset": s.end(), "joined": true})));
    }
    map.insert(id.clone(), session);
    drop(map);
    // The container's own console starts at whatever size LXC gave it.
    let _ = size_console(&cts, &id, cols, rows);
    Ok(Json(json!({"name": id, "offset": 0, "joined": false})))
}

#[derive(Deserialize)]
struct ReadQ {
    #[serde(default)]
    after: u64,
}

async fn read(
    Extension(cts): Extension<Manager>,
    Path(name): Path<String>,
    Query(q): Query<ReadQ>,
) -> Result<Json<Value>, ApiError> {
    let id = id_of(&cts, &name)?;
    let s = live(&id).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    s.read(q.after).await.map(Json).map_err(ApiError::internal)
}

#[derive(Deserialize)]
struct WriteReq {
    /// Base64, so control characters and raw bytes survive JSON.
    data: String,
}

async fn write(
    Extension(cts): Extension<Manager>,
    Path(name): Path<String>,
    Json(req): Json<WriteReq>,
) -> Result<Json<Value>, ApiError> {
    let id = id_of(&cts, &name)?;
    let s = live(&id).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    s.write_b64(&req.data).map_err(ApiError::bad_request)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ResizeReq {
    cols: u16,
    rows: u16,
}

/// Write a window size onto the container's own console, which is what the
/// shell and `apt` inside read. `lxc_console` does not carry our pty's size
/// across its proxy (`Manager::console_fd`), so both are set: ours, so
/// `lxc-console` itself agrees, and the container's.
fn size_console(cts: &Manager, id: &str, cols: u16, rows: u16) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = cts.console_fd(id)?;
    // O_NOCTTY: never adopt this tty as our controlling terminal.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    pty::set_winsize(file.as_raw_fd(), cols, rows)
}

async fn resize(
    Extension(cts): Extension<Manager>,
    Path(name): Path<String>,
    Json(req): Json<ResizeReq>,
) -> Result<Json<Value>, ApiError> {
    let id = id_of(&cts, &name)?;
    let s = live(&id).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    let (cols, rows) = (req.cols.max(1), req.rows.max(1));
    s.resize(cols, rows).map_err(ApiError::internal)?;
    let c = cts.clone();
    tokio::task::spawn_blocking(move || size_console(&c, &id, cols, rows))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"cols": cols, "rows": rows})))
}

async fn disconnect(
    Extension(cts): Extension<Manager>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = id_of(&cts, &name)?;
    if let Some(s) = sessions().lock().expect("console sessions").remove(&id) {
        s.close();
    }
    Ok(Json(json!({"name": id, "closed": true})))
}
