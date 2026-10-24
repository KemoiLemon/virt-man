//! A login shell on the daemon's host, for the browser.
//!
//! The PTY runs the host's own `login(1)`, so the terminal *is* the login UI:
//! it prints `login:` and `Password:`, reads them with echo off, authenticates
//! through the host's PAM stack, and finally execs the account's login shell.
//! The daemon holds no credentials, runs no PAM conversation of its own, and
//! decides nothing about who may log in — `login` and the host's account
//! policy do. That also means it works whether or not the daemon itself runs
//! as root: `login` is setuid-root on the distributions that ship it that way,
//! and when the daemon *is* root (its normal mode) the child simply starts as
//! root and `login` drops to the authenticated user.
//!
//! The transport is the long-poll pty in `crate::pty`, the same one the
//! container console uses. A session belongs to the page that opened it: a
//! random id names it, its pty is private, and it closes when the browser
//! leaves or after an idle window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use crate::pty::{self, Session};

/// Shells one daemon will run at once. A page is one session, and the idle
/// timeout reaps abandoned ones, so a handful is generous.
const MAX_SESSIONS: usize = 4;
/// The login program. `login(1)` is the host's own interactive PAM front end;
/// it is what prints the prompt, verifies the password and starts the shell.
const LOGIN: &str = "/bin/login";

fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static S: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn routes() -> Router {
    Router::new()
        .route("/shell/connect", post(connect))
        .route("/shell/{id}/read", get(read))
        .route("/shell/{id}/write", post(write))
        .route("/shell/{id}/resize", post(resize))
        .route("/shell/{id}/disconnect", post(disconnect))
}

fn live(id: &str) -> Option<Arc<Session>> {
    let map = sessions().lock().expect("shell sessions");
    map.get(id).filter(|s| !s.is_closed()).cloned()
}

#[derive(Deserialize)]
struct ConnectReq {
    /// The browser's initial terminal size. A pty left at 0x0 makes `login`'s
    /// line editor redraw one line; 80x24 is the sane default until the first
    /// resize arrives.
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Open a login session: start `login(1)` in a pty and hand back its id. The
/// prompt and the password are entirely between the terminal and `login`.
async fn connect(Json(req): Json<ConnectReq>) -> Result<Json<Value>, ApiError> {
    if sessions().lock().expect("shell sessions").len() >= MAX_SESSIONS {
        return Err(ApiError::conflict(format!(
            "too many shells are already open (limit {MAX_SESSIONS}); close one first"
        )));
    }
    let id = pty::random_id();
    let cols = req.cols.unwrap_or(80).max(1);
    let rows = req.rows.unwrap_or(24).max(1);
    let gone = id.clone();
    // A deliberately bare environment; the real one is built by `login`.
    let session = pty::spawn(
        &id,
        &[LOGIN],
        &["TERM=xterm-256color"],
        cols,
        rows,
        move || {
            sessions().lock().expect("shell sessions").remove(&gone);
        },
    )
    .map_err(ApiError::internal)?;
    let mut map = sessions().lock().expect("shell sessions");
    // Two connects can pass the check above together; the loser is closed
    // rather than left running unlisted.
    if map.len() >= MAX_SESSIONS {
        session.close();
        return Err(ApiError::conflict("too many shells are already open"));
    }
    map.insert(id.clone(), session);
    Ok(Json(json!({"id": id, "offset": 0})))
}

/// A session id names a live shell in the map, for the integration test.
#[doc(hidden)]
#[allow(dead_code)]
pub fn session_by_id(id: &str) -> bool {
    live(id).is_some()
}

#[derive(Deserialize)]
struct ReadQ {
    #[serde(default)]
    after: u64,
}

async fn read(Path(id): Path<String>, Query(q): Query<ReadQ>) -> Result<Json<Value>, ApiError> {
    let s = live(&id).ok_or_else(|| ApiError::not_found("no shell session; connect first"))?;
    s.read(q.after).await.map(Json).map_err(ApiError::internal)
}

#[derive(Deserialize)]
struct WriteReq {
    /// Base64, so control characters and raw bytes survive JSON.
    data: String,
}

async fn write(Path(id): Path<String>, Json(req): Json<WriteReq>) -> Result<Json<Value>, ApiError> {
    let s = live(&id).ok_or_else(|| ApiError::not_found("no shell session; connect first"))?;
    s.write_b64(&req.data).map_err(ApiError::bad_request)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ResizeReq {
    cols: u16,
    rows: u16,
}

async fn resize(
    Path(id): Path<String>,
    Json(req): Json<ResizeReq>,
) -> Result<Json<Value>, ApiError> {
    let s = live(&id).ok_or_else(|| ApiError::not_found("no shell session; connect first"))?;
    s.resize(req.cols, req.rows).map_err(ApiError::internal)?;
    Ok(Json(json!({"cols": req.cols, "rows": req.rows})))
}

/// Close the session now. The browser fires this from `pagehide` too, so
/// leaving the page kills the shell without waiting for the idle timeout.
async fn disconnect(Path(id): Path<String>) -> Json<Value> {
    if let Some(s) = sessions().lock().expect("shell sessions").remove(&id) {
        s.close();
    }
    Json(json!({"id": id, "closed": true}))
}
