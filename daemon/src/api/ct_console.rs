//! A container's text console, for the browser.
//!
//! An LXC guest has no display: its console is a pty, reached through
//! `virDomainOpenConsole` as a libvirt stream. `POST …/console/connect` opens
//! one and returns a session id; `GET …/console/read?after=N` long-polls for
//! the bytes since offset `N` (parked until there are some, like the VNC
//! frame route, so no client-side poll interval); `POST …/console/write`
//! sends keystrokes. The proxy cannot carry a WebSocket, so this is the same
//! request/response shape the rest of the console uses.
//!
//! Measured against libvirt 12.7: the LXC driver refuses
//! `VIR_DOMAIN_CONSOLE_FORCE` ("unsupported flags (0x1)"), so a console is
//! opened without it and a second viewer is refused rather than stealing the
//! first one's session. That is why a session is shared per container: two
//! tabs on one container read the same session instead of fighting.
//!
//! Output is kept in a bounded ring (the last [`KEEP`] bytes), so a reader
//! that falls behind loses the oldest bytes, never memory. A session closes
//! when nobody has read it for [`IDLE`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use super::containers::lxc;

pub const IDLE: Duration = Duration::from_secs(30);
pub const KEEP: usize = 256 * 1024;
/// A read parks at most this long before answering "nothing new".
const PARK: Duration = Duration::from_secs(25);

struct Session {
    /// Everything received, trimmed from the front to `KEEP`; `base` is the
    /// stream offset of `buf[0]`.
    out: Mutex<(u64, Vec<u8>)>,
    ready: Condvar,
    input: Mutex<Vec<u8>>,
    last_read: Mutex<Instant>,
    closed: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Session {
    fn new() -> Self {
        Self {
            out: Mutex::new((0, Vec::new())),
            ready: Condvar::new(),
            input: Mutex::new(Vec::new()),
            last_read: Mutex::new(Instant::now()),
            closed: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut o = self.out.lock().expect("console out");
        o.1.extend_from_slice(bytes);
        if o.1.len() > KEEP {
            let cut = o.1.len() - KEEP;
            o.1.drain(..cut);
            o.0 += cut as u64;
        }
        self.ready.notify_all();
    }

    /// Bytes from offset `after`, the offset after them, and whether any were
    /// lost off the front of the ring.
    fn since(&self, after: u64) -> (Vec<u8>, u64, bool) {
        let o = self.out.lock().expect("console out");
        let end = o.0 + o.1.len() as u64;
        let from = after.max(o.0).min(end);
        let start = (from - o.0) as usize;
        (o.1[start..].to_vec(), end, after < o.0)
    }

    fn end(&self) -> u64 {
        let o = self.out.lock().expect("console out");
        o.0 + o.1.len() as u64
    }
}

fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static S: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn routes() -> Router {
    Router::new()
        .route("/containers/{name}/console/connect", post(connect))
        .route("/containers/{name}/console/read", get(read))
        .route("/containers/{name}/console/write", post(write))
        .route("/containers/{name}/console/disconnect", post(disconnect))
}

fn live(name: &str) -> Option<Arc<Session>> {
    let map = sessions().lock().expect("console sessions");
    map.get(name)
        .filter(|s| !s.closed.load(Ordering::Relaxed))
        .cloned()
}

/// Open (or join) the container's console.
async fn connect(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    if let Some(s) = live(&name) {
        return Ok(Json(
            json!({"name": name, "offset": s.end(), "joined": true}),
        ));
    }
    let s = Arc::new(Session::new());
    let (n, sess) = (name.clone(), s.clone());
    // The stream belongs to one connection on one thread for its whole life:
    // opened, read, written and closed there.
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    lxc(async move {
        // `with_conn` would close the connection when the closure returns, so
        // the console thread opens its own and owns it.
        std::thread::spawn(move || pump(&n, &sess, tx));
    })
    .await;
    rx.await
        .map_err(|_| ApiError::internal("console thread ended"))?
        .map_err(ApiError::bad_request)?;
    sessions()
        .lock()
        .expect("console sessions")
        .insert(name.clone(), s);
    Ok(Json(json!({"name": name, "offset": 0, "joined": false})))
}

/// Open the console stream and move bytes both ways until the container
/// stops, the session goes idle, or someone disconnects.
fn pump(name: &str, s: &Session, ready: tokio::sync::oneshot::Sender<Result<(), String>>) {
    use virt::connect::Connect;
    use virt::stream::Stream;
    let mut conn = match Connect::open(Some(crate::libvirt::LXC_URI)) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!("libvirt connect: {e}")));
            return;
        }
    };
    let opened = (|| -> Result<Stream, String> {
        let d = crate::libvirt::domain(&conn, name).map_err(|e| e.to_string())?;
        if !d.is_active().unwrap_or(false) {
            return Err("the container is not running".into());
        }
        let st = Stream::new(&conn, virt::sys::VIR_STREAM_NONBLOCK).map_err(|e| e.to_string())?;
        // No FORCE: the LXC driver refuses the flag outright.
        d.open_console(None, &st, 0).map_err(|e| e.to_string())?;
        Ok(st)
    })();
    let st = match opened {
        Ok(st) => {
            let _ = ready.send(Ok(()));
            st
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            let _ = conn.close();
            return;
        }
    };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        if s.closed.load(Ordering::Relaxed)
            || s.last_read.lock().expect("console idle").elapsed() > IDLE
        {
            break;
        }
        let pending = std::mem::take(&mut *s.input.lock().expect("console input"));
        if !pending.is_empty() {
            let mut sent = 0;
            while sent < pending.len() {
                match st.send(&pending[sent..]) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                    Ok(n) => sent += n,
                    // -2: would block, on a non-blocking stream.
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
        match st.recv(&mut buf) {
            Ok(0) => {
                *s.error.lock().expect("console err") = Some("the console closed".into());
                break;
            }
            Ok(n) => s.push(&buf[..n]),
            // Nothing to read yet (a non-blocking stream answers -2).
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = st.abort();
    s.closed.store(true, Ordering::Relaxed);
    s.ready.notify_all();
    let _ = conn.close();
}

#[derive(Deserialize)]
struct ReadQ {
    #[serde(default)]
    after: u64,
}

async fn read(Path(name): Path<String>, Query(q): Query<ReadQ>) -> Result<Json<Value>, ApiError> {
    let s = live(&name).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    *s.last_read.lock().expect("console idle") = Instant::now();
    let s2 = s.clone();
    // Park until there is something past `after`, the session ends, or PARK.
    let (bytes, next, lost) = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + PARK;
        let mut o = s2.out.lock().expect("console out");
        loop {
            let end = o.0 + o.1.len() as u64;
            if end > q.after || s2.closed.load(Ordering::Relaxed) {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            o = s2.ready.wait_timeout(o, left).expect("console wait").0;
        }
        drop(o);
        s2.since(q.after)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    *s.last_read.lock().expect("console idle") = Instant::now();
    Ok(Json(json!({
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "offset": next,
        "lost": lost,
        "closed": s.closed.load(Ordering::Relaxed),
        "error": s.error.lock().expect("console err").clone(),
    })))
}

#[derive(Deserialize)]
struct WriteReq {
    /// Base64, so control characters and raw bytes survive JSON.
    data: String,
}

async fn write(
    Path(name): Path<String>,
    Json(req): Json<WriteReq>,
) -> Result<Json<Value>, ApiError> {
    let s = live(&name).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(req.data.as_bytes())
        .map_err(|_| ApiError::bad_request("data must be base64"))?;
    if bytes.len() > 64 * 1024 {
        return Err(ApiError::bad_request("too much input at once"));
    }
    s.input.lock().expect("console input").extend(bytes);
    Ok(Json(json!({"ok": true})))
}

async fn disconnect(Path(name): Path<String>) -> Json<Value> {
    if let Some(s) = sessions().lock().expect("console sessions").remove(&name) {
        s.closed.store(true, Ordering::Relaxed);
        s.ready.notify_all();
    }
    Json(json!({"name": name, "closed": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes_and_reports_a_gap() {
        let s = Session::new();
        s.push(&vec![b'a'; KEEP]);
        s.push(b"xyz");
        let (b, next, lost) = s.since(0);
        assert!(lost, "the first three bytes fell off");
        assert_eq!(b.len(), KEEP);
        assert!(b.ends_with(b"xyz"));
        assert_eq!(next, KEEP as u64 + 3);
        let (b, _, lost) = s.since(next - 2);
        assert_eq!((b.as_slice(), lost), (&b"yz"[..], false));
        let (b, _, _) = s.since(next + 100);
        assert!(b.is_empty(), "a reader ahead of the stream gets nothing");
    }
}
