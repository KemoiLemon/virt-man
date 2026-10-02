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
use crate::libvirt::with_conn;

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
        .route("/containers/{name}/console/resize", post(resize))
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

#[derive(Deserialize)]
struct ResizeReq {
    cols: u16,
    rows: u16,
}

/// Set the container's console pty to the browser terminal's size.
///
/// libvirt opens the LXC console as a pty but never sets its window size, so
/// the kernel reports `0x0` and the shell's line editor (and every full-screen
/// program) redraws against a terminal with no geometry: at the bottom it
/// overwrites the same line instead of scrolling, and long lines never wrap
/// where the browser wraps them. libvirt's own API has no resize call, but the
/// pty is a host device and this daemon is on the host, so `TIOCSWINSZ` on it
/// is the whole fix -- the same ioctl a terminal emulator would issue.
///
/// The pty path comes from the domain's XML (`<console type='pty'>` names its
/// `<source path>`), which is why this needs the domain, not the stream.
async fn resize(
    Path(name): Path<String>,
    Json(req): Json<ResizeReq>,
) -> Result<Json<Value>, ApiError> {
    // A resize on a console nobody is watching is meaningless, and it would
    // open a pty for a container that may have stopped.
    live(&name).ok_or_else(|| ApiError::not_found("no console session; connect first"))?;
    let (cols, rows) = (req.cols.max(1), req.rows.max(1));
    let path = lxc(async move {
        with_conn(move |c| {
            let d = crate::libvirt::domain(c, &name).map_err(|e| e.to_string())?;
            // LIVE XML, not INACTIVE: libvirt only records the console pty's
            // path (`<source path='/dev/pts/N'/>`) while the domain runs. The
            // inactive definition has just `<console type='pty'>` with no
            // source, which is why this read the wrong flag and found nothing.
            let xml = d.get_xml_desc(0).map_err(|e| e.to_string())?;
            Ok(console_pty(&xml).ok_or_else(|| "the container has no console pty".to_string())?)
        })
        .await
    })
    .await
    .map_err(ApiError::internal)?;
    set_pty_size(&path, cols, rows).map_err(ApiError::internal)?;
    Ok(Json(json!({"cols": cols, "rows": rows})))
}

/// The `<source path='/dev/pts/N'/>` (or the `tty` attribute) of a domain's
/// console pty, or the pty target. None when the domain has no pty console.
fn console_pty(xml: &str) -> Option<String> {
    let i = xml.find("<console")?;
    let block = &xml[i..];
    let end = block.find('>').map(|j| j + 1).unwrap_or(block.len());
    let open = &block[..end];
    // A self-closing `<console type='pty' tty='/dev/pts/N'/>` carries it as an
    // attribute; otherwise the `<source path=.../>` inside does.
    attr(open, "tty")
        .or_else(|| {
            let rest = &block[end..];
            let j = rest.find("</console>").map(|j| j + 8).unwrap_or(rest.len());
            attr(&rest[..j], "path")
        })
        .filter(|p| p.starts_with("/dev/"))
}

/// The value of `name='...'` or `name="..."` in an XML tag, unescaped.
fn attr(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(name)?;
    // Only a whole attribute name, not `sometty` for `tty`.
    let before = tag[..at].chars().next_back();
    if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let rest = tag[at + name.len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let v = &rest[1..];
    let end = v.find(quote)?;
    Some(crate::libvirt::unescape(&v[..end]))
}

/// Write a window size onto an open pty. `TIOCSWINSZ` is the ioctl a terminal
/// emulator uses; the kernel then tells the foreground process (`SIGWINCH`) and
/// reports the size back through `TIOCGWINSZ`.
fn set_pty_size(path: &str, cols: u16, rows: u16) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOCTTY: never adopt this pty as our controlling terminal.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .map_err(|e| format!("open {path}: {e}"))?;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `ws` is a valid winsize and the fd is open for the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if rc != 0 {
        return Err(format!(
            "TIOCSWINSZ {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
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

    #[test]
    fn the_console_pty_is_read_from_either_shape() {
        // libvirt reports a running LXC console as a self-closing element with
        // the pty on `tty`, and some versions carry it in a `<source>`.
        let running = "<domain><devices><console type='pty' tty='/dev/pts/7'>\
            <source path='/dev/pts/7'/><target type='lxc' port='0'/></console></devices></domain>";
        assert_eq!(console_pty(running).as_deref(), Some("/dev/pts/7"));
        let sourced = "<console type='pty'><source path='/dev/pts/12'/></console>";
        assert_eq!(console_pty(sourced).as_deref(), Some("/dev/pts/12"));
        // A domain with no console, and a tty that is not a device path.
        assert_eq!(console_pty("<domain><devices/></domain>"), None);
        assert_eq!(console_pty("<console type='pty' tty='console'/>"), None);
    }

    #[test]
    fn attr_matches_a_whole_name_only() {
        assert_eq!(attr("<console tty='/dev/pts/3'/>", "tty").as_deref(), Some("/dev/pts/3"));
        assert_eq!(attr("<console tty=\"/dev/pts/3\"/>", "tty").as_deref(), Some("/dev/pts/3"));
        // `sometty` must not answer for `tty`, and a missing name is None.
        assert_eq!(attr("<console sometty='/x'/>", "tty"), None);
        assert_eq!(attr("<console type='pty'/>", "tty"), None);
    }
}
