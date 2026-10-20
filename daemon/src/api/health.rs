use std::process::Command;

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

pub fn routes() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
}

/// Liveness only: the daemon answers, so the web server's probe is up. This
/// path must not touch QEMU, and neither does it.
async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn version() -> Json<Value> {
    Json(json!({
        "daemon": env!("CARGO_PKG_VERSION"),
        "qemu": qemu_version(),
    }))
}

/// The first line of `qemu-system-x86_64 --version`, which names the QEMU
/// release the daemon launches. `unavailable` when the binary is not there, so
/// the version answer still works on a host before install.
fn qemu_version() -> String {
    let out = Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .or_else(|_| {
            Command::new("/usr/bin/qemu-system-x86_64")
                .arg("--version")
                .output()
        });
    match out {
        Ok(out) if out.status.success() => first_line(&out.stdout),
        _ => "unavailable".into(),
    }
}

/// The first line of the program's output, trimmed.
fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("unavailable")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_answers_ok_without_a_hypervisor() {
        let Json(body) = health().await;
        assert_eq!(body, json!({"status": "ok"}));
    }

    #[test]
    fn the_qemu_version_is_the_first_line() {
        assert_eq!(
            first_line(
                b"QEMU emulator version 9.2.0 (Debian 1:9.2.0+ds-2)\nCopyright (c) 2003-2024\n"
            ),
            "QEMU emulator version 9.2.0 (Debian 1:9.2.0+ds-2)"
        );
        assert_eq!(first_line(b"\n\n"), "unavailable");
    }

    /// The answer names the daemon and QEMU.
    #[tokio::test]
    async fn version_reports_qemu_only() {
        let Json(body) = version().await;
        assert_eq!(body["daemon"], env!("CARGO_PKG_VERSION"));
        assert!(body["qemu"].as_str().is_some_and(|v| !v.is_empty()));
    }
}
