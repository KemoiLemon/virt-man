//! A shell on the daemon's host, for the browser.
//!
//! This is deliberately not a guest console. `POST /shell/connect` takes a
//! host username and password, verifies them through PAM, and only then starts
//! a login shell in a pty as that exact user — uid, gid, supplementary groups
//! and home included. The session is owned by the page that opened it: a
//! random id names it, `DELETE`/`POST …/disconnect` (the browser also beacons
//! one when it unloads) tears down the process group, and an idle session with
//! nobody reading it closes itself. Nothing is shared between sessions.
//!
//! The transport is the same long-poll the container console uses — no
//! WebSocket, because the proxy cannot carry one: `read?after=N` parks until
//! there are bytes past offset `N`, `write` sends keystrokes, `resize` carries
//! the browser terminal's geometry. Output is kept in a bounded ring, so a
//! reader that falls behind loses the oldest bytes rather than memory.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query};
use axum::http::StatusCode;
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
    user: String,
    password: String,
    /// The browser's initial terminal size. A pty left at 0x0 makes the
    /// shell's line editor redraw one line; 80x24 is the sane default until
    /// the first resize arrives.
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// Authenticate through PAM, then start a login shell as that user.
async fn connect(Json(req): Json<ConnectReq>) -> Result<Json<Value>, ApiError> {
    let user = req.user.trim().to_string();
    if user.is_empty() {
        return Err(ApiError::bad_request("a username is required"));
    }
    if sessions().lock().expect("shell sessions").len() >= MAX_SESSIONS {
        return Err(ApiError::conflict(format!(
            "too many shells are already open (limit {MAX_SESSIONS}); close one first"
        )));
    }
    // PAM reads the host's shadow database and may run helper programs, so it
    // belongs on a blocking thread rather than a runtime worker.
    let (u, p) = (user.clone(), req.password);
    tokio::task::spawn_blocking(move || crate::pam::authenticate(&u, &p))
        .await
        .map_err(|e| ApiError::internal(format!("shell auth task: {e}")))?
        .map_err(|e| ApiError::new(StatusCode::UNAUTHORIZED, e))?;

    let account = lookup_user(&user).map_err(ApiError::bad_request)?;
    let id = random_id();
    let cols = req.cols.unwrap_or(80).max(1);
    let rows = req.rows.unwrap_or(24).max(1);
    let session = start_shell(&id, &user, &account, cols, rows).map_err(ApiError::internal)?;
    let mut map = sessions().lock().expect("shell sessions");
    // Two connects can pass the check above together; the loser is closed
    // rather than left running unlisted.
    if map.len() >= MAX_SESSIONS {
        close_session(&session);
        return Err(ApiError::conflict("too many shells are already open"));
    }
    map.insert(id.clone(), session);
    Ok(Json(json!({"id": id, "user": user, "offset": 0})))
}

/// One host account, resolved before the fork so the child does no lookups.
#[derive(Debug)]
struct Account {
    uid: libc::uid_t,
    gid: libc::gid_t,
    home: String,
    shell: String,
    groups: Vec<libc::gid_t>,
}

fn lookup_user(name: &str) -> Result<Account, String> {
    let cname = CString::new(name).map_err(|_| "invalid username".to_string())?;
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: pwd is zeroed and getpwnam_r fills it plus buf; result
        // points at pwd on success or is null.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            if buf.len() > 1 << 20 {
                return Err(format!("the account record for {name} is unreasonably large"));
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(format!(
                "could not look up {name}: {}",
                std::io::Error::from_raw_os_error(rc)
            ));
        }
        if result.is_null() {
            return Err(format!("no such host user: {name}"));
        }
        let home = cstr(pwd.pw_dir);
        let shell = match cstr(pwd.pw_shell) {
            s if s.is_empty() => "/bin/sh".to_string(),
            s => s,
        };
        let groups = lookup_groups(&cname, pwd.pw_gid);
        return Ok(Account {
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
            home,
            shell,
            groups,
        });
    }
}

/// The user's supplementary groups, primary included — what `initgroups`
/// would produce, resolved here so the child only has to call `setgroups`.
fn lookup_groups(name: &CString, gid: libc::gid_t) -> Vec<libc::gid_t> {
    let mut n: c_int = 16;
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
    if rc < 0 {
        groups.resize(n.max(1) as usize, 0);
        unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
    }
    groups.truncate((n.max(0) as usize).min(groups.len()));
    groups
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null char pointer from a passwd record is a C string
        // owned by the libc lookup buffer, which is still alive here.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Start the shell in its own pty and a thread that pumps it.
fn start_shell(
    id: &str,
    user: &str,
    account: &Account,
    cols: u16,
    rows: u16,
) -> Result<Arc<Session>, String> {
    // Everything the child needs is built here, before fork: after fork a
    // multithreaded process must not allocate or look anything up.
    let shell = CString::new(account.shell.as_str()).map_err(|_| "invalid shell path")?;
    let argv0 = CString::new(login_argv0(&account.shell)).map_err(|_| "invalid shell path")?;
    let cwd = CString::new(account.home.as_str()).map_err(|_| "invalid home directory")?;
    let env: Vec<CString> = shell_env(user, &account.home, &account.shell)
        .into_iter()
        .filter_map(|s| CString::new(s).ok())
        .collect();
    let argv: [*const c_char; 2] = [argv0.as_ptr(), std::ptr::null()];
    let mut envp: Vec<*const c_char> = env.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());

    // A CLOEXEC pipe: the child writes its failure reason and exits, or exec
    // succeeds and closes the write end, leaving the parent with an EOF.
    let mut pipe = [0 as RawFd; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
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
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if pid < 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        return Err(format!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child. Everything the shell needs is already built; only
        // async-signal-safe calls happen here, and nothing may allocate.
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
            // Dropping to the authenticated user needs privilege: `setgroups`
            // (and `setuid` to another account) require root or CAP_SETGID /
            // CAP_SETUID. A daemon running as an ordinary user is refused here
            // with a clear message rather than starting the shell as the
            // daemon's own user, which would be the wrong account.
            if libc::setgid(account.gid) != 0 {
                child_fail(3, "setgid (the daemon must run as root for the shell)");
            }
            if libc::setgroups(account.groups.len(), account.groups.as_ptr()) != 0 {
                child_fail(3, "setgroups (the daemon must run as root for the shell)");
            }
            if libc::setuid(account.uid) != 0 {
                child_fail(3, "setuid (the daemon must run as root for the shell)");
            }
            if libc::chdir(cwd.as_ptr()) != 0 {
                child_fail(3, "chdir");
            }
            libc::execve(shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
            child_fail(3, "exec");
        }
    }
    // Parent: closes the write end so the read below sees EOF on a clean exec.
    unsafe { libc::close(pipe[1]) };
    let mut reason = [0u8; 256];
    let n = unsafe { libc::read(pipe[0], reason.as_mut_ptr() as *mut c_void, reason.len()) };
    unsafe { libc::close(pipe[0]) };
    if n != 0 {
        let msg = if n > 0 {
            format!("could not start the shell: {}", String::from_utf8_lossy(&reason[..n as usize]))
        } else {
            format!("could not start the shell: {}", std::io::Error::last_os_error())
        };
        unsafe {
            libc::close(master);
            libc::kill(pid, libc::SIGKILL);
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        return Err(msg);
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
            format!("shell thread: {e}")
        })?;
    Ok(session)
}

/// Move bytes both ways until the shell exits, the session goes idle, or
/// someone disconnects; then close the pty and the whole process group.
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
                // last slave closes. Both mean the shell is gone.
                let gone = n == 0
                    || !matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    );
                if gone {
                    *s.error.lock().expect("shell err") = Some("the shell exited".into());
                    break;
                }
            }
        }
    }
    unsafe { libc::close(fd) };
    // Signal the whole session, not just the shell, so background jobs a user
    // started do not outlive the page that opened them.
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
        libc::write(
            status_fd,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
        );
        libc::_exit(127)
    }
}

/// A login shell is invoked with a leading `-` on argv[0]; that is what makes
/// it read the login profiles. `bash` also takes `-l`, but not every shell
/// does, and this spelling is what `login(1)` uses.
fn login_argv0(shell: &str) -> String {
    let base = shell.rsplit('/').next().unwrap_or(shell);
    format!("-{base}")
}

/// A deliberately small environment. The daemon's own would leak paths and
/// settings that belong to the service, not to the person logging in.
fn shell_env(user: &str, home: &str, shell: &str) -> Vec<String> {
    vec![
        format!("HOME={home}"),
        format!("USER={user}"),
        format!("LOGNAME={user}"),
        format!("SHELL={shell}"),
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        "TERM=xterm-256color".into(),
    ]
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
    fn a_login_shell_is_argv0_with_a_leading_dash() {
        assert_eq!(login_argv0("/bin/bash"), "-bash");
        assert_eq!(login_argv0("/usr/bin/zsh"), "-zsh");
        assert_eq!(login_argv0("sh"), "-sh");
    }

    #[test]
    fn the_environment_names_the_user_and_their_home() {
        let env = shell_env("alice", "/home/alice", "/bin/bash");
        for want in [
            "HOME=/home/alice",
            "USER=alice",
            "LOGNAME=alice",
            "SHELL=/bin/bash",
        ] {
            assert!(env.iter().any(|e| e == want), "missing {want} in {env:?}");
        }
        // The daemon's own environment must not ride along.
        assert!(env.iter().all(|e| !e.starts_with("VIRT_MAN_")));
    }

    #[test]
    fn session_ids_are_random_hex() {
        let a = random_id();
        let b = random_id();
        assert_eq!(a.len(), 32, "16 bytes of hex: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two sessions must not share an address");
    }

    #[test]
    fn an_unknown_host_user_is_reported_not_guessed() {
        let err = lookup_user("virt-man-no-such-user-zzz").unwrap_err();
        assert!(err.contains("no such host user"), "got: {err}");
    }

    /// The child parks its error-pipe write end at fd 3 before `close_range`,
    /// so a failure to start is still reported instead of looking like a clean
    /// exec. Prove a real forkpty+exec runs and that a non-existent shell
    /// reports through that pipe rather than silently "succeeding".
    ///
    /// Dropping to another user needs privilege (`setgroups`/`setuid`), so this
    /// only runs where those calls are permitted — root, or a namespace with
    /// `/proc/self/setgroups` = allow. `sudo -E cargo test --bins -- --ignored`.
    #[test]
    #[ignore = "needs privilege to setgroups/setuid to another user"]
    fn a_shell_starts_and_a_bad_one_reports_through_the_pipe() {
        // A real account we can rely on in a test: the current user.
        let me = unsafe { libc::getpwuid(libc::geteuid()) };
        assert!(!me.is_null());
        let name = unsafe { CStr::from_ptr((*me).pw_name) }
            .to_string_lossy()
            .into_owned();
        let mut account = lookup_user(&name).expect("the running user exists");
        account.groups = vec![account.gid];

        let ok = match start_shell("unit-ok", &name, &account, 80, 24) {
            Ok(s) => s,
            // A kernel/user namespace that forbids `setgroups` cannot run this
            // path at all; that is an environment limit, not a shell bug, and
            // the child's message names it.
            Err(e) if e.contains("setgroups") => {
                eprintln!("skipping: this environment forbids setgroups ({e})");
                return;
            }
            Err(e) => panic!("start the login shell: {e}"),
        };
        // A shell with stdin as a pty echoes nothing until given input; just
        // confirm it is alive and then tear it down.
        assert!(!ok.closed.load(Ordering::Relaxed));
        close_session(&ok);

        // A shell path that does not exist must fail loudly, not appear to
        // start: this is what the fd-3 parking exists to guarantee.
        account.shell = "/nonexistent/virt-man-shell".into();
        let err = match start_shell("unit-bad", &name, &account, 80, 24) {
            Err(e) => e,
            Ok(_) => panic!("a non-existent shell must not appear to start"),
        };
        assert!(
            err.contains("could not start the shell") && !err.contains("setgroups"),
            "a failed exec is reported: {err}"
        );
    }
}
