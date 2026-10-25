//! A program in its own pty, pumped for the browser.
//!
//! Both text terminals are this: the host shell runs `login(1)` (`api::shell`)
//! and a container's console runs `lxc-console` against it
//! (`api::ct_console`). The transport is a long-poll, not a WebSocket, because
//! the web proxy cannot carry one: a read parks until there are bytes past an
//! offset, a write queues keystrokes, a resize sets the pty's window. Output is
//! kept in a bounded ring, so a reader that falls behind loses the oldest bytes
//! rather than memory. A session nobody has read for [`IDLE`] closes, and
//! closing it hangs up the whole process group.

use std::ffi::{CString, c_char, c_void};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

/// A session with nobody reading it for this long closes.
pub const IDLE: Duration = Duration::from_secs(30);
/// Output kept per session; older bytes are dropped, with `lost` reporting it.
pub const KEEP: usize = 256 * 1024;
/// A read parks at most this long before answering "nothing new".
const PARK: Duration = Duration::from_secs(25);
/// Largest keystroke batch accepted.
pub const MAX_INPUT: usize = 64 * 1024;

pub struct Session {
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
    fn new(fd: RawFd, pid: libc::pid_t) -> Self {
        Self {
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
        let mut o = self.out.lock().expect("pty out");
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
        let o = self.out.lock().expect("pty out");
        let end = o.0 + o.1.len() as u64;
        let from = after.max(o.0).min(end);
        let start = (from - o.0) as usize;
        (o.1[start..].to_vec(), end, after < o.0)
    }

    /// The offset a newly joining reader starts from.
    pub fn end(&self) -> u64 {
        let o = self.out.lock().expect("pty out");
        o.0 + o.1.len() as u64
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Park until there is something past `after`, the session ends, or the
    /// park window passes, then answer the read the browser polls.
    pub async fn read(self: &Arc<Self>, after: u64) -> Result<Value, String> {
        *self.last_read.lock().expect("pty idle") = Instant::now();
        let s = self.clone();
        let (bytes, next, lost) = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + PARK;
            let mut o = s.out.lock().expect("pty out");
            loop {
                let end = o.0 + o.1.len() as u64;
                if end > after || s.closed.load(Ordering::Relaxed) {
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                o = s.ready.wait_timeout(o, left).expect("pty wait").0;
            }
            drop(o);
            s.since(after)
        })
        .await
        .map_err(|e| e.to_string())?;
        *self.last_read.lock().expect("pty idle") = Instant::now();
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "offset": next,
            "lost": lost,
            "closed": self.is_closed(),
            "error": self.error.lock().expect("pty err").clone(),
        }))
    }

    /// Queue base64 keystrokes for the pump.
    pub fn write_b64(&self, data: &str) -> Result<(), String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|_| "data must be base64".to_string())?;
        if bytes.len() > MAX_INPUT {
            return Err("too much input at once".into());
        }
        self.input.lock().expect("pty input").extend(bytes);
        Ok(())
    }

    /// Set the pty's window. The kernel tells the foreground process group
    /// (`SIGWINCH`) and reports the size back through `TIOCGWINSZ`.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        set_winsize(self.fd, cols, rows)
    }

    /// Close now: the pump notices, closes the pty and hangs up the group.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = unsafe { libc::kill(-self.pid, libc::SIGHUP) };
        self.ready.notify_all();
    }
}

/// Start `argv` (an absolute program path first) in its own pty, and a thread
/// that pumps it until it exits, goes idle or is closed; `ended` runs after
/// that, so the caller can drop the session from its map.
pub fn spawn(
    name: &str,
    argv: &[&str],
    env: &[&str],
    cols: u16,
    rows: u16,
    ended: impl FnOnce() + Send + 'static,
) -> Result<Arc<Session>, String> {
    let cargs: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a).map_err(|_| format!("argument {a:?} has a NUL")))
        .collect::<Result<_, _>>()?;
    let cenv: Vec<CString> = env
        .iter()
        .map(|a| CString::new(*a).map_err(|_| format!("environment {a:?} has a NUL")))
        .collect::<Result<_, _>>()?;
    let program = cargs.first().ok_or("no program to run")?.clone();
    let mut argv_p: Vec<*const c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    // argv[0] is the program's base name, so `login` does not mistake itself
    // for a shell invoked with a leading dash.
    let base = CString::new(argv[0].rsplit('/').next().unwrap_or(argv[0]).to_string())
        .map_err(|e| e.to_string())?;
    argv_p[0] = base.as_ptr();
    argv_p.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = cenv.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());

    // A CLOEXEC pipe: the child writes its failure reason and exits, or exec
    // succeeds and closes the write end, leaving the parent with an EOF.
    let mut pipe = [0 as RawFd; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
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
        return Err(format!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // Child. Only async-signal-safe calls: nothing here may allocate or
        // lock -- the daemon is multithreaded.
        unsafe {
            libc::close(pipe[0]);
            // The error pipe's write end must survive the close_range below, so
            // it is parked at fd 3 first. `close_range` then drops every other
            // descriptor above stderr, so the program cannot inherit the
            // daemon's sockets or another session's pty; fd 0/1/2 are the pty
            // slave forkpty already pointed at us. On a kernel without
            // close_range the call fails harmlessly -- the daemon's own fds are
            // CLOEXEC.
            if pipe[1] != 3 {
                libc::dup2(pipe[1], 3);
                libc::close(pipe[1]);
            }
            // dup2 clears close-on-exec on the copy. Without setting it back
            // the program inherits the write end, the parent's read below
            // never sees EOF, and connect hangs for as long as it runs.
            libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 4u32);
            libc::execve(program.as_ptr(), argv_p.as_ptr(), envp.as_ptr());
            child_fail(3, b"could not start the terminal program");
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
        return Err(format!("{msg}: {}", argv[0]));
    }
    // Non-blocking so a full pty never stalls the pump; CLOEXEC so a later
    // forkpty cannot hand it to another session's child.
    set_fd_flags(master);

    let session = Arc::new(Session::new(master, pid));
    let pumped = session.clone();
    std::thread::Builder::new()
        .name(format!("pty-{name}"))
        .spawn(move || {
            pump(&pumped);
            ended();
        })
        .map_err(|e| {
            session.close();
            format!("terminal thread: {e}")
        })?;
    Ok(session)
}

/// Move bytes both ways until the program exits, the session goes idle, or
/// someone closes it; then close the pty and the whole process group.
fn pump(s: &Session) {
    let (fd, pid) = (s.fd, s.pid);
    let mut buf = vec![0u8; 16 * 1024];
    // Why the loop ended, for the log: a session that dies the instant it
    // starts is otherwise indistinguishable from one the page closed.
    let mut why = "the program exited or hung up the terminal";
    let mut detail = String::new();
    let mut exited: Option<i32> = None;
    let mut gap: Option<Instant> = None;
    'pump: loop {
        if s.closed.load(Ordering::Relaxed) {
            why = "closed by the client";
            break;
        }
        if s.last_read.lock().expect("pty idle").elapsed() > IDLE {
            why = "nobody read it for the idle timeout";
            break;
        }
        let pending = std::mem::take(&mut *s.input.lock().expect("pty input"));
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
                gap = None;
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
                    // EOF/EIO means no process has the slave open *right now*.
                    // A login that reopens its terminal (a hangup and a fresh
                    // open) leaves a gap like that while it is still running, so
                    // it is only the end if the program has actually exited.
                    // Short gaps are waited out; a long one is a dead session.
                    let mut st = 0;
                    let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
                    if r == 0 {
                        let since = *gap.get_or_insert_with(Instant::now);
                        if since.elapsed() < Duration::from_secs(3) {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        detail =
                            format!("terminal gone for 3s, program still running (read {n}, {e})");
                    } else {
                        exited = (r == pid).then_some(st);
                        detail = format!("read {n} ({e})");
                    }
                    *s.error.lock().expect("pty err") = Some("the session ended".into());
                    break;
                }
            }
        }
    }
    unsafe { libc::close(fd) };
    // Signal the whole session, not just its leader, so background jobs a
    // user started do not outlive the page that opened them.
    let _ = unsafe { libc::kill(-pid, libc::SIGHUP) };
    let status = exited.unwrap_or_else(|| reap(pid));
    tracing::info!(pid, status, "pty session ended: {why} {detail}");
    s.closed.store(true, Ordering::Relaxed);
    s.ready.notify_all();
}

/// Wait briefly for the child, then force it. A program that traps SIGHUP
/// gets half a second, not forever.
fn reap(pid: libc::pid_t) -> i32 {
    let mut status = 0;
    for _ in 0..25 {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid || r < 0 {
            return status;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    status
}

/// Report why the child could not start, then die. Only a write and `_exit`,
/// which are safe after fork.
unsafe fn child_fail(status_fd: RawFd, why: &'static [u8]) -> ! {
    unsafe {
        libc::write(status_fd, why.as_ptr() as *const c_void, why.len());
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

/// Write a window size onto a tty fd. `TIOCSWINSZ` is the ioctl a terminal
/// emulator uses.
pub fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> Result<(), String> {
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
/// never address another's terminal.
pub fn random_id() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_bytes_and_reports_a_gap() {
        let s = Session::new(-1, -1);
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

    /// A real program in a real pty: its output arrives through the ring,
    /// keystrokes reach it, and the session closes when it exits.
    #[tokio::test]
    async fn a_program_is_pumped_both_ways_and_ends() {
        let s = spawn("test", &["/bin/cat"], &[], 80, 24, || {}).unwrap();
        s.write_b64(&base64::engine::general_purpose::STANDARD.encode(b"hello\n"))
            .unwrap();
        let mut got = String::new();
        let mut after = 0;
        for _ in 0..20 {
            let v = s.read(after).await.unwrap();
            after = v["offset"].as_u64().unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap())
                .unwrap();
            got.push_str(&String::from_utf8_lossy(&bytes));
            if got.matches("hello").count() >= 2 {
                break;
            }
        }
        assert!(
            got.matches("hello").count() >= 2,
            "echo and cat's copy: {got:?}"
        );
        s.close();
        for _ in 0..50 {
            if s.is_closed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(s.is_closed());
        assert!(spawn("bad", &["/nonexistent/prog"], &[], 80, 24, || {}).is_err());
    }
}
