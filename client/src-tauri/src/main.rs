#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tauri::Manager;

// ---------------------------------------------------------------- NVIDIA fix
// Same workaround as vm-man: WebKitGTK + proprietary NVIDIA renders blank
// windows and can die on Wayland with Error 71. Set before WebKitGTK inits.
#[cfg(target_os = "linux")]
fn apply_gpu_workarounds() {
    let nvidia = Path::new("/proc/driver/nvidia/version").exists()
        || Path::new("/dev/nvidiactl").exists()
        || fs::read_dir("/sys/module")
            .map(|mut d| d.any(|e| e.map(|e| e.file_name() == "nvidia").unwrap_or(false)))
            .unwrap_or(false);

    match std::env::var("VIRT_MAN_GPU_WORKAROUND").as_deref() {
        Ok("0") | Ok("off") => return,
        Ok("1") | Ok("force") => {}
        _ if !nvidia => return,
        _ => {}
    }

    let set = |k: &str, v: &str| {
        if std::env::var_os(k).is_none() {
            // SAFETY: single-threaded, before any GUI/webkit init.
            unsafe { std::env::set_var(k, v) };
        }
    };
    set("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    set("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    set("__NV_DISABLE_EXPLICIT_SYNC", "1");
}

#[cfg(not(target_os = "linux"))]
fn apply_gpu_workarounds() {}

// ---------------------------------------------------------------- sidecar

/// Find the virt-man-web binary. Installed layout: next to the app binary
/// (/opt/virt-man/virt-man-web); dev: repo target/debug; fallback: PATH.
fn sidecar_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["virt-man-web", "virt-man-web.exe"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    for name in ["/usr/local/bin/virt-man-web", "/usr/bin/virt-man-web"] {
        let p = Path::new(name);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    // Dev fallback: repo layout target/debug for both crates.
    let cargo = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let root = Path::new(&cargo).parent()?.parent()?; // client/src-tauri -> client -> repo
    let bin = root.join("web/server/target/debug/virt-man-web");
    if bin.is_file() {
        return Some(bin);
    }
    None
}

fn free_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

fn data_dir() -> PathBuf {
    std::env::var("VIRT_MAN_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/share/virt-man")
        })
}

/// Locate the built SPA. Precedence: env var, dev repo layout, installed
/// layout next to the app binary (/opt/virt-man/dist).
fn frontend_dist() -> String {
    if let Ok(d) = std::env::var("VIRT_MAN_WEB_DIST") {
        if Path::new(&d).is_dir() {
            return d;
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let dev = Path::new(&manifest)
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("web/dist"));
        if let Some(d) = dev.filter(|d| d.is_dir()) {
            return d.to_string_lossy().into_owned();
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let local = dir.join("dist");
            if local.is_dir() {
                return local.to_string_lossy().into_owned();
            }
            let share = dir.join("../share/virt-man/dist");
            if share.is_dir() {
                return share.to_string_lossy().into_owned();
            }
        }
    }
    for base in ["/usr/local/share/virt-man/dist", "/usr/share/virt-man/dist"] {
        let p = Path::new(base);
        if p.is_dir() {
            return base.to_string();
        }
    }
    String::new()
}

fn spawn_backend() -> (Child, u16) {
    let port = free_port().expect("no free loopback port");
    let dist = frontend_dist();
    let bin = sidecar_binary().expect("virt-man-web backend not found");
    let child = Command::new(bin)
        .env("VIRT_MAN_WEB_LOOPBACK_HTTP", "1")
        .env("VIRT_MAN_WEB_BIND", "127.0.0.1")
        .env("VIRT_MAN_WEB_PORT", port.to_string())
        .env("VIRT_MAN_WEB_DATA", data_dir())
        .env("VIRT_MAN_WEB_DIST", dist)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn virt-man-web");
    (child, port)
}

fn wait_healthy(port: u16) -> bool {
    for _ in 0..50 {
        if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            use std::io::{Read, Write};
            let _ = stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

struct Backend(std::sync::Mutex<Option<Child>>);

fn run() {
    apply_gpu_workarounds();

    let (child, port) = spawn_backend();
    if !wait_healthy(port) {
        panic!("virt-man-web backend did not become healthy");
    }

    let app = tauri::Builder::default()
        .manage(Backend(std::sync::Mutex::new(Some(child))))
        .setup(move |app| {
            let url = format!("http://127.0.0.1:{port}");
            // First paint: load the web UI in the main window.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.navigate(url.parse().expect("valid url"));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(state) = window.app_handle().try_state::<Backend>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(mut c) = guard.take() {
                            let _ = c.kill();
                        }
                    }
                }
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_handle, _event| {});
}

fn main() {
    run();
}
