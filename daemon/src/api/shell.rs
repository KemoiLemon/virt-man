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
//! The transport is the same long-poll the container console uses — no
//! WebSocket, because the proxy cannot carry one: `read?after=N` parks until
//! there are bytes past offset `N`, `write` sends keystrokes, `resize` carries
//! the browser terminal's geometry. Output is kept in a bounded ring, so a
//! reader that falls behind loses the oldest bytes rather than memory. A
//! session belongs to the page that opened it: a random id names it, its pty
//! is private, and it closes when the browser leaves or after an idle window.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::os::fd::RawFd;
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

/// A session with nobody reading it for this long closes.
pub const IDLE: Duration = Duration::from_secs(30);
/// Output kept per session; older bytes are dropped, with `lost` reporting it.
pub const KEEP: usize = 256 * 1024;
/// A read parks at most this long before answering "nothing new".
const PARK: Duration = Duration::from_secs(25);
/// Shells one daemon will run at once. A page is one session, and the idle
/// timeout reaps abandoned ones, so a handful is generous.
const MAX_SESSIONS: usize = 4;
/// Largest keystroke batch accepted, matching the console.
const MAX_INPUT: usize = 64 * 1024;
/// The login program. `login(1)` is the host's own interactive PAM front end;
/// it is what prints the prompt, verifies the password and starts the shell.
const LOGIN: &str = "/bin/login";

struct Session {
    id: String,
    fd: RawFd,
    pid: libc::pid_t,
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
    fn new(id: String, fd: RawFd, pid: libc::pid_t) -> Self {
        Self {
            id,
            fd,
            pid,
            out: Mutex::new((0, Vec::new())),
            ready: Condvar::new(),
            input: Mutex::new(Vec::new()),
            last_read: Mutex::new(Instant::now()),
            closed: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut o = self.out.lock().expect("shell out");
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
        let o = self.out.lock().expect("shell out");
        let end = o.0 + o.1.len() as u64;
        let from = after.max(o.0).min(end);
        let start = (from - o.0) as usize;
        (o.1[start..].to_vec(), end, after < o.0)
    }
}

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
    map.get(id)
        .filter(|s| !s.closed.load(Ordering::Relaxed))
        .cloned()
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
    let id = random_id();
    let cols = req.cols.unwrap_or(80).max(1);
    let rows = req.rows.unwrap_or(24).max(1);
    let session = start_login(&id, cols, rows)?;
    let mut map = sessions().lock().expect("shell sessions");
    // Two connects can pass the check above together; the loser is closed
    // rather than left running unlisted.
    if map.len() >= MAX_SESSIONS {
        close_session(&session);
        return Err(ApiError::conflict("too many shells are already open"));
    }
    map.insert(id.clone(), session);
    Ok(Json(json!({"id": id, "offset": 0})))
}

/// Start `login(1)` in its own pty and a thread that pumps it.
fn start_login(id: &str, cols: u16, rows: u16) -> Result<Arc<Session>, ApiError> {
    let login = CString::new(LOGIN).expect("a static path");
    // argv[0] = "login" so the program does not mistake itself for a shell
    // invoked with a leading dash.
    let argv0 = CString::new("login").expect("a static string");
    let argv: [*const c_char; 2] = [argv0.as_ptr(), std::ptr::null()];
    // A deliberately bare environment; the real one is built by `login`.
    let term = CString::new("TERM=xterm-256color").expect("a static string");
    let envp: [*const c_char; 2] = [term.as_ptr(), std::ptr::null()];

    // A CLOEXEC pipe: the child writes its failure reason and exits, or exec
    // succeeds and closes the write end, leaving the parent with an EOF.
    let mut pipe = [0 as RawFd; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(ApiError::internal(format!(
            "pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: forkpty opens the pty, forks and makes the child a session
    // leader with the pty as its controlling terminal; the child below only
    // calls async-signal-safe functions before execve.
    let mut master: RawFd = -1;
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if pid < 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        return Err(ApiError::internal(format!(
            "forkpty: {}",
            std::io::Error::last_os_error()
        )));
    }
    if pid == 0 {
        // Child. Only async-signal-safe calls: nothing here may allocate or
        // lock -- the daemon is multithreaded.
        unsafe {
            libc::close(pipe[0]);
            // The error pipe's write end must survive the close_range below, so
            // it is parked at fd 3 first. `close_range` then drops every other
            // descriptor above stderr, so the shell cannot inherit the daemon's
            // sockets or another session's pty; fd 0/1/2 are the pty slave
            // forkpty already pointed at us. On a kernel without close_range
            // the call fails harmlessly -- the daemon's own fds are CLOEXEC.
            if pipe[1] != 3 {
                libc::dup2(pipe[1], 3);
                libc::close(pipe[1]);
            }
            libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 4u32);
            libc::execve(login.as_ptr(), argv.as_ptr(), envp.as_ptr());
            child_fail(3, "could not start login(1)");
        }
    }
    // Parent: closes the write end so the read below sees EOF on a clean exec.
    unsafe { libc::close(pipe[1]) };
    let mut reason = [0u8; 256];
    let n = unsafe { libc::read(pipe[0], reason.as_mut_ptr() as *mut c_void, reason.len()) };
    unsafe { libc::close(pipe[0]) };
    if n != 0 {
        let msg = if n > 0 {
            String::from_utf8_lossy(&reason[..n as usize]).into_owned()
        } else {
            std::io::Error::last_os_error().to_string()
        };
        unsafe {
            libc::close(master);
            libc::kill(pid, libc::SIGKILL);
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        return Err(ApiError::internal(msg));
    }
    // Non-blocking so a full pty never stalls the pump; CLOEXEC so a later
    // forkpty cannot hand it to another session's child.
    set_fd_flags(master);

    let session = Arc::new(Session::new(id.to_string(), master, pid));
    let pumped = session.clone();
    std::thread::Builder::new()
        .name(format!("shell-{id}"))
        .spawn(move || pump(pumped))
        .map_err(|e| {
            close_session(&session);
            ApiError::internal(format!("shell thread: {e}"))
        })?;
    Ok(session)
}

/// Move bytes both ways until `login` (and the shell it starts) exits, the
/// session goes idle, or someone disconnects; then close the pty and the whole
/// process group.
fn pump(s: Arc<Session>) {
    let (fd, pid) = (s.fd, s.pid);
    let mut buf = vec![0u8; 16 * 1024];
    'pump: loop {
        if s.closed.load(Ordering::Relaxed)
            || s.last_read.lock().expect("shell idle").elapsed() > IDLE
        {
            break;
        }
        let pending = std::mem::take(&mut *s.input.lock().expect("shell input"));
        if !pending.is_empty() {
            let mut sent = 0;
            while sent < pending.len() {
                let n = unsafe {
                    libc::write(
                        fd,
                        pending[sent..].as_ptr() as *const c_void,
                        pending.len() - sent,
                    )
                };
                if n > 0 {
                    sent += n as usize;
                } else {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    } else {
                        break 'pump;
                    }
                }
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 20) };
        if rc > 0 && pfd.revents != 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n > 0 {
                s.push(&buf[..n as usize]);
            } else {
                let e = std::io::Error::last_os_error();
                // n == 0 is EOF; on a pty master Linux reports EIO once the
                // last slave closes. Both mean the session is gone.
                let gone = n == 0
                    || !matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    );
                if gone {
                    *s.error.lock().expect("shell err") = Some("the session ended".into());
                    break;
                }
            }
        }
    }
    unsafe { libc::close(fd) };
    // Signal the whole session, not just login/shell, so background jobs a
    // user started do not outlive the page that opened them.
    let _ = unsafe { libc::kill(-pid, libc::SIGHUP) };
    reap(pid);
    s.closed.store(true, Ordering::Relaxed);
    s.ready.notify_all();
    sessions().lock().expect("shell sessions").remove(&s.id);
}

/// Wait briefly for the child, then force it. A shell that traps SIGHUP gets
/// half a second, not forever.
fn reap(pid: libc::pid_t) {
    let mut status = 0;
    for _ in 0..25 {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid || r < 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
}

fn close_session(s: &Session) {
    s.closed.store(true, Ordering::Relaxed);
    let _ = unsafe { libc::kill(-s.pid, libc::SIGHUP) };
    s.ready.notify_all();
}

/// Report why the child could not start, then die. Only a write and `_exit`,
/// which are safe after fork.
unsafe fn child_fail(status_fd: RawFd, why: &'static str) -> ! {
    let bytes = why.as_bytes();
    unsafe {
        libc::write(status_fd, bytes.as_ptr() as *const c_void, bytes.len());
        libc::_exit(127)
    }
}

fn set_fd_flags(fd: RawFd) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> Result<(), String> {
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// A page-private session id. Random rather than sequential so one caller can
/// never address another's shell.
fn random_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
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
    *s.last_read.lock().expect("shell idle") = Instant::now();
    let s2 = s.clone();
    // Park until there is something past `after`, the session ends, or PARK.
    let (bytes, next, lost) = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + PARK;
        let mut o = s2.out.lock().expect("shell out");
        loop {
            let end = o.0 + o.1.len() as u64;
            if end > q.after || s2.closed.load(Ordering::Relaxed) {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            o = s2.ready.wait_timeout(o, left).expect("shell wait").0;
        }
        drop(o);
        s2.since(q.after)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    *s.last_read.lock().expect("shell idle") = Instant::now();
    Ok(Json(json!({
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "offset": next,
        "lost": lost,
        "closed": s.closed.load(Ordering::Relaxed),
        "error": s.error.lock().expect("shell err").clone(),
    })))
}

#[derive(Deserialize)]
struct WriteReq {
    /// Base64, so control characters and raw bytes survive JSON.
    data: String,
}

async fn write(Path(id): Path<String>, Json(req): Json<WriteReq>) -> Result<Json<Value>, ApiError> {
    let s = live(&id).ok_or_else(|| ApiError::not_found("no shell session; connect first"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(req.data.as_bytes())
        .map_err(|_| ApiError::bad_request("data must be base64"))?;
    if bytes.len() > MAX_INPUT {
        return Err(ApiError::bad_request("too much input at once"));
    }
    s.input.lock().expect("shell input").extend(bytes);
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
    set_winsize(s.fd, req.cols, req.rows).map_err(ApiError::internal)?;
    Ok(Json(json!({"cols": req.cols, "rows": req.rows})))
}

/// Close the session now. The browser fires this from `pagehide` too, so
/// leaving the page kills the shell without waiting for the idle timeout.
async fn disconnect(Path(id): Path<String>) -> Json<Value> {
    if let Some(s) = sessions().lock().expect("shell sessions").remove(&id) {
        close_session(&s);
    }
    Json(json!({"id": id, "closed": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes_and_reports_a_gap() {
        let s = Session::new("t".into(), -1, -1);
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
    fn session_ids_are_random_hex() {
        let a = random_id();
        let b = random_id();
        assert_eq!(a.len(), 32, "16 bytes of hex: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two sessions must not share an address");
    }
}
