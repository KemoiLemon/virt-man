use serde::Deserialize;
use std::ffi::CStr;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// HTTP API bind address, e.g. `127.0.0.1:8443`.
    pub http_addr: String,
    /// Unix socket path. When unset, derived from whether we run as root.
    pub unix_socket: Option<String>,
    /// File containing the pre-generated auth token.
    pub token_path: PathBuf,
    /// Directory for the self-signed TLS certificate/key.
    pub cert_dir: PathBuf,
    pub qemu_state_root: PathBuf,
    pub data_dir: PathBuf,
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// If both are set, serve these instead of the generated certificate.
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8443".into(),
            unix_socket: None,
            token_path: PathBuf::from("/etc/virt-man-daemon/token"),
            cert_dir: PathBuf::from("/etc/virt-man-daemon/certs"),
            qemu_state_root: PathBuf::from("/var/lib/libvirt-daemon"),
            data_dir: PathBuf::from("/var/lib/virt-man-daemon"),
            tls: TlsConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let explicit = std::env::var_os("VIRT_MAN_CONFIG").map(PathBuf::from);
        if explicit.is_none() {
            migrate_legacy_paths()?;
        }
        let path = explicit.unwrap_or_else(|| {
            let new = PathBuf::from("/etc/virt-man-daemon/daemon.toml");
            if new.exists() {
                new
            } else {
                PathBuf::from("/etc/virt-man/daemon.toml")
            }
        });
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            let mut cfg: Config = toml::from_str(&text)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
            rewrite_legacy_config(&mut cfg);
            Ok(cfg)
        } else {
            Ok(Config::default())
        }
    }

    pub fn unix_socket_path(&self) -> PathBuf {
        if let Some(p) = &self.unix_socket {
            return PathBuf::from(p);
        }
        if unsafe { libc::geteuid() } == 0 {
            PathBuf::from("/run/virt-man-daemon/daemon.sock")
        } else {
            std::env::var("XDG_RUNTIME_DIR")
                .map(|d| PathBuf::from(d).join("virt-man/daemon.sock"))
                .unwrap_or_else(|_| PathBuf::from("/tmp/virt-man-daemon.sock"))
        }
    }

    /// Parsed bind address, with the IPv4 wildcard mapped to the IPv6 one so a
    /// single socket serves both families. `localhost` resolves to `::1` on
    /// many systems, so a `0.0.0.0` listener refuses `https://localhost:8443`
    /// outright while `127.0.0.1` works.
    ///
    /// Only the wildcard is rewritten: an explicit `127.0.0.1` stays
    /// loopback-only, since widening a deliberately narrow bind would be a
    /// security change. Needs `net.ipv6.bindv6only=0` (the Linux default).
    pub fn http_socket_addr(&self) -> Result<std::net::SocketAddr, String> {
        dual_stack_addr(&self.http_addr)
    }

    pub fn cert_path(&self) -> PathBuf {
        self.cert_dir.join("cert.pem")
    }

    pub fn key_path(&self) -> PathBuf {
        self.cert_dir.join("key.pem")
    }

    /// Paths to the TLS certificate to serve, preferring configured ones
    /// over the generated ones.
    pub fn tls_cert_key(&self) -> (PathBuf, PathBuf) {
        match (&self.tls.cert, &self.tls.key) {
            (Some(c), Some(k)) => (c.clone(), k.clone()),
            _ => (self.cert_path(), self.key_path()),
        }
    }
}

/// Copy legacy daemon-owned configuration once without following destination
/// symlinks or replacing operator-provided files.
pub fn migrate_legacy_paths() -> Result<(), String> {
    migrate_paths(
        PathBuf::from("/etc/virt-man"),
        PathBuf::from("/etc/virt-man-daemon"),
    )
}

fn migrate_paths(old_root: PathBuf, new_root: PathBuf) -> Result<(), String> {
    const FILES: &[&str] = &[
        "daemon.toml",
        "token",
        "certs/cert.pem",
        "certs/key.pem",
        "tls/cert.pem",
        "tls/key.pem",
        "firewall.json",
        "idmap.json",
        "titles.json",
        "volume-titles.json",
    ];
    for relative in FILES {
        let old = old_root.join(relative);
        let new = new_root.join(relative);
        let source_meta = match std::fs::symlink_metadata(&old) {
            Ok(m) if m.file_type().is_file() => m,
            Ok(_) => {
                tracing::warn!(path=%old.display(), "skipping non-regular legacy config source");
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", old.display())),
        };
        if let Some(parent) = new.parent() {
            create_private_dirs(parent)?;
        }
        let mut source = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&old)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", old.display())),
        };
        let mut dest = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(file_mode(relative, source_meta.permissions().mode()))
            .custom_flags(libc::O_NOFOLLOW)
            .open(&new)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", new.display())),
        };
        std::io::copy(&mut source, &mut dest).map_err(|e| format!("{}: {e}", new.display()))?;
        if relative == &"daemon.toml" {
            dest.sync_all()
                .map_err(|e| format!("{}: {e}", new.display()))?;
            drop(dest);
            transform_config_paths(&new)?;
            continue;
        }
        dest.sync_all()
            .map_err(|e| format!("{}: {e}", new.display()))?;
        tracing::info!(from = %old.display(), to = %new.display(), "migrated legacy daemon file");
    }
    Ok(())
}

use std::os::unix::fs::OpenOptionsExt;
fn file_mode(relative: &str, source: u32) -> u32 {
    if relative == "token" || relative.ends_with("key.pem") {
        source & 0o600
    } else {
        source & 0o640
    }
}
fn create_private_dirs(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_dir() => continue,
            Ok(_) => return Err(format!("{} is not a real directory", current.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut b = std::fs::DirBuilder::new();
                b.mode(0o700);
                b.create(&current)
                    .map_err(|e| format!("{}: {e}", current.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", current.display())),
        }
    }
    Ok(())
}
fn transform_config_paths(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut doc: toml::Value =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    for field in ["token_path", "cert_dir"] {
        rewrite_toml_path(&mut doc, field);
    }
    if let Some(tls) = doc.get_mut("tls").and_then(toml::Value::as_table_mut) {
        for field in ["cert", "key"] {
            if let Some(v) = tls.get_mut(field) {
                rewrite_path_value(v);
            }
        }
    }
    let text = toml::to_string(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!(
        "toml.migrate.{}.{}.tmp",
        std::process::id(),
        std::thread::current().name().unwrap_or("thread")
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    use std::io::Write;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    let mode = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o777;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::File::open(path.parent().ok_or("config path has no parent")?)
        .and_then(|d| d.sync_all())
        .map_err(|e| e.to_string())
}
fn rewrite_toml_path(doc: &mut toml::Value, key: &str) {
    if let Some(v) = doc.get_mut(key) {
        rewrite_path_value(v);
    }
}
fn rewrite_path_value(v: &mut toml::Value) {
    if let Some(s) = v.as_str() {
        let n = match s {
            "/etc/virt-man/token" => "/etc/virt-man-daemon/token",
            "/etc/virt-man/certs" => "/etc/virt-man-daemon/certs",
            "/etc/virt-man/tls/cert.pem" => "/etc/virt-man-daemon/tls/cert.pem",
            "/etc/virt-man/tls/key.pem" => "/etc/virt-man-daemon/tls/key.pem",
            _ => return,
        };
        *v = toml::Value::String(n.into());
    }
}
fn rewrite_legacy_config(cfg: &mut Config) {
    if cfg.token_path == std::path::Path::new("/etc/virt-man/token") {
        cfg.token_path = "/etc/virt-man-daemon/token".into();
    }
    if cfg.cert_dir == std::path::Path::new("/etc/virt-man/certs") {
        cfg.cert_dir = "/etc/virt-man-daemon/certs".into();
    }
    if cfg.tls.cert.as_deref() == Some(std::path::Path::new("/etc/virt-man/tls/cert.pem")) {
        cfg.tls.cert = Some("/etc/virt-man-daemon/tls/cert.pem".into());
    }
    if cfg.tls.key.as_deref() == Some(std::path::Path::new("/etc/virt-man/tls/key.pem")) {
        cfg.tls.key = Some("/etc/virt-man-daemon/tls/key.pem".into());
    }
}

/// Rewrite a `0.0.0.0:port` bind to `[::]:port`, then parse. Anything else is
/// parsed as given.
pub fn dual_stack_addr(addr: &str) -> Result<std::net::SocketAddr, String> {
    let rewritten = match addr.strip_prefix("0.0.0.0:") {
        Some(port) => format!("[::]:{port}"),
        None => addr.to_string(),
    };
    rewritten
        .parse()
        .map_err(|e| format!("invalid http_addr {addr}: {e}"))
}

/// Reject startup unless running as root or in the `libvirt` group.
pub fn ensure_privileges() -> Result<(), String> {
    if is_privileged() {
        Ok(())
    } else {
        Err("virt-man-daemon must run as root or as a user in the `libvirt` group".to_string())
    }
}

fn is_privileged() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    // Primary group first (getgroups() only lists supplementary groups).
    if group_name(unsafe { libc::getegid() }) == "libvirt" {
        return true;
    }
    supplementary_groups()
        .iter()
        .any(|g| group_name(*g) == "libvirt")
}

fn supplementary_groups() -> Vec<libc::gid_t> {
    unsafe {
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n <= 0 {
            return Vec::new();
        }
        let mut groups = vec![0 as libc::gid_t; n as usize];
        let got = libc::getgroups(n, groups.as_mut_ptr());
        if got <= 0 {
            return Vec::new();
        }
        groups.truncate(got as usize);
        groups
    }
}

/// getgrgid returns a static pointer and is not thread-safe; we only call it
/// at startup before any threads spawn.
fn group_name(gid: libc::gid_t) -> String {
    unsafe {
        let grp = libc::getgrgid(gid);
        if grp.is_null() {
            String::new()
        } else {
            CStr::from_ptr((*grp).gr_name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dual_stack_addr;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn migration_is_exclusive_idempotent_and_transforms_config_paths() {
        let root = std::env::temp_dir().join(format!(
            "vm-config-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("token"), b"same-token\n").unwrap();
        std::fs::write(
            old.join("daemon.toml"),
            "token_path='/etc/virt-man/token'\n",
        )
        .unwrap();
        super::migrate_paths(old.clone(), new.clone()).unwrap();
        assert_eq!(std::fs::read(new.join("token")).unwrap(), b"same-token\n");
        let parsed: toml::Value =
            toml::from_str(&std::fs::read_to_string(new.join("daemon.toml")).unwrap()).unwrap();
        assert_eq!(
            parsed["token_path"].as_str(),
            Some("/etc/virt-man-daemon/token")
        );
        std::fs::write(new.join("token"), b"new-token").unwrap();
        super::migrate_paths(old, new.clone()).unwrap();
        assert_eq!(std::fs::read(new.join("token")).unwrap(), b"new-token");
        assert_eq!(
            std::fs::metadata(new.join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_does_not_follow_destination_symlink() {
        let root = std::env::temp_dir().join(format!(
            "vm-config-link-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("token"), b"legacy").unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, new.join("token")).unwrap();
        super::migrate_paths(old, new).unwrap();
        assert_eq!(std::fs::read(victim).unwrap(), b"unchanged");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_config_values_are_rewritten_without_touching_custom_values() {
        let mut cfg = super::Config {
            token_path: "/etc/virt-man/token".into(),
            cert_dir: "/custom/certs".into(),
            tls: super::TlsConfig {
                cert: Some("/etc/virt-man/tls/cert.pem".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        super::rewrite_legacy_config(&mut cfg);
        assert_eq!(
            cfg.token_path,
            std::path::Path::new("/etc/virt-man-daemon/token")
        );
        assert_eq!(cfg.cert_dir, std::path::Path::new("/custom/certs"));
        assert_eq!(
            cfg.tls.cert.unwrap(),
            std::path::Path::new("/etc/virt-man-daemon/tls/cert.pem")
        );
    }

    #[test]
    fn the_ipv4_wildcard_becomes_dual_stack() {
        // 0.0.0.0 refuses ::1, which is what `localhost` resolves to on many
        // systems, so the wildcard must bind [::] to serve both families.
        let a = dual_stack_addr("0.0.0.0:8443").unwrap();
        assert!(a.is_ipv6(), "wildcard binds IPv6 ({a})");
        assert_eq!(a.port(), 8443);
        assert!(a.ip().is_unspecified(), "still a wildcard");
    }

    #[test]
    fn an_explicit_loopback_bind_is_left_alone() {
        // Narrowing is intentional; do not silently widen it.
        let a = dual_stack_addr("127.0.0.1:8443").unwrap();
        assert!(a.is_ipv4());
        assert_eq!(a.to_string(), "127.0.0.1:8443");
    }

    #[test]
    fn a_specific_address_is_left_alone() {
        assert_eq!(
            dual_stack_addr("192.168.0.119:8443").unwrap().to_string(),
            "192.168.0.119:8443"
        );
    }

    #[test]
    fn an_ipv6_bind_passes_through() {
        assert!(dual_stack_addr("[::]:8443").unwrap().is_ipv6());
        assert!(dual_stack_addr("[::1]:8443").unwrap().is_ipv6());
    }

    #[test]
    fn a_bad_address_reports_the_original_text() {
        let err = dual_stack_addr("not-an-addr").unwrap_err();
        assert!(err.contains("not-an-addr"), "error names the input: {err}");
    }
}
