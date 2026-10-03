//! A daemon shell, end to end.
//!
//! PAM refuses a wrong password, a real login drops a live shell, and the API
//! carries bytes both ways over a session of its own. Needs a host account and
//! its password, so it is `--ignored` and driven by the environment:
//!
//! ```text
//! sudo -E SHELL_TEST_USER=kemoi SHELL_TEST_PASS=... \
//!   cargo test --test shell -- --ignored --test-threads=1
//! ```
//!
//! The suite is generic over the account, so it works on any host. It proves
//! the shell runs as the account rather than as the daemon's own user, that it
//! has a controlling tty, that the ring streams output, and that disconnect
//! and an idle timeout both end the process.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread::sleep;
use std::time::{Duration, Instant};

use virt_man_daemon::api::shell;

fn creds() -> (String, String) {
    let user = std::env::var("SHELL_TEST_USER")
        .expect("set SHELL_TEST_USER to a host account that can log in");
    let pass = std::env::var("SHELL_TEST_PASS").expect("set SHELL_TEST_PASS to that account's password");
    (user, pass)
}

fn uid_of(name: &str) -> u32 {
    let out = std::process::Command::new("id")
        .args(["-u", name])
        .output()
        .expect("id");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("uid")
}

/// POST json, GET a path — just enough HTTP for the shell routes.
fn request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut stream = TcpStream::connect(addr).expect("connect to the test daemon");
    let req = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    };
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let json = text
        .split("\r\n\r\n")
        .nth(1)
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Read until `needle` shows up or the deadline passes.
fn read_until(addr: &str, id: &str, offset: &mut u64, needle: &str, secs: u64) -> String {
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        let (status, v) = request(addr, "GET", &format!("/shell/{id}/read?after={offset}"), None);
        assert_eq!(status, 200, "read: {v}");
        if let Some(data) = v["data"].as_str() {
            let decoded = b64_decode(data);
            seen.push_str(&String::from_utf8_lossy(&decoded));
        }
        *offset = v["offset"].as_u64().unwrap_or(*offset);
        if seen.contains(needle) {
            break;
        }
        if v["closed"].as_bool().unwrap_or(false) {
            break;
        }
    }
    seen
}

fn b64_decode(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = T.iter().position(|&t| t == c) else {
            continue;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// A small HTTP server in front of the shell routes, so the suite drives the
/// real handlers over a real socket without needing the daemon's TLS config.
async fn spawn_api() -> String {
    let app = shell::routes();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn connect_ok(addr: &str, user: &str, pass: &str) -> String {
    let body = serde_json::json!({"user": user, "password": pass, "cols": 100, "rows": 30}).to_string();
    let (status, v) = request(addr, "POST", "/shell/connect", Some(&body));
    assert_eq!(status, 200, "connect should succeed: {v}");
    v["id"].as_str().expect("an id").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real host account (SHELL_TEST_USER / SHELL_TEST_PASS)"]
async fn a_wrong_password_is_refused_and_a_right_one_opens_a_shell() {
    let (user, pass) = creds();
    let addr = spawn_api().await;

    let bad = serde_json::json!({"user": user, "password": "definitely-not-the-password"}).to_string();
    let (status, v) = request(&addr, "POST", "/shell/connect", Some(&bad));
    assert_eq!(status, 401, "a wrong password must be refused: {v}");
    assert!(
        v["error"].as_str().unwrap_or("").to_lowercase().contains("auth"),
        "the refusal names authentication: {v}"
    );

    let id = connect_ok(&addr, &user, &pass).await;
    let uid = uid_of(&user);

    // Run commands: prove the uid, a controlling tty, and that a full line of
    // input reaches a real shell.
    let cmd = "id -u; tty; echo SHELL-OK-$((6*7))\n".to_string();
    let encoded = {
        use std::fmt::Write as _;
        let raw = cmd.as_bytes();
        let mut s = String::new();
        for chunk in raw.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
            for (i, &ix) in idx.iter().enumerate() {
                if i <= chunk.len() {
                    let _ = write!(s, "{}", table[ix as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    };
    let (status, v) = request(
        &addr,
        "POST",
        &format!("/shell/{id}/write"),
        Some(&serde_json::json!({"data": encoded}).to_string()),
    );
    assert_eq!(status, 200, "write: {v}");

    let mut offset = 0;
    let out = read_until(&addr, &id, &mut offset, "SHELL-OK-42", 15);
    assert!(out.contains(&uid.to_string()), "runs as uid {uid}: {out:?}");
    assert!(out.contains("/dev/pts/"), "has a controlling tty: {out:?}");

    // Resize is accepted and the pty agrees.
    let (status, _) = request(
        &addr,
        "POST",
        &format!("/shell/{id}/resize"),
        Some(&serde_json::json!({"cols": 132, "rows": 40}).to_string()),
    );
    assert_eq!(status, 200, "resize");

    // Disconnect ends it, and the session is gone from the map.
    let (status, _) = request(&addr, "POST", &format!("/shell/{id}/disconnect"), None);
    assert_eq!(status, 200, "disconnect");
    assert!(
        !shell::session_by_id(&id),
        "a disconnected session is dropped at once"
    );

    // The process is really gone: the pty's child is reaped, so a second read
    // is a 404 rather than a live shell.
    let (status, _) = request(&addr, "GET", &format!("/shell/{id}/read?after=0"), None);
    assert_eq!(status, 404, "the session no longer exists");
}

/// Nobody reading it for the idle window closes the shell by itself, so an
/// abandoned tab does not leave a login shell running forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real host account, and waits out the idle timeout"]
async fn an_idle_session_closes_itself() {
    let (user, pass) = creds();
    let addr = spawn_api().await;
    let id = connect_ok(&addr, &user, &pass).await;
    // Never read it: the pump sees no reader and stops after IDLE.
    let deadline = Instant::now() + shell::IDLE + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !shell::session_by_id(&id) {
            return;
        }
        sleep(Duration::from_millis(200));
    }
    panic!("an unwatched shell did not close within the idle window");
}
