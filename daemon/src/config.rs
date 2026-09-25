use serde::Deserialize;
use std::ffi::CStr;
use std::path::PathBuf;

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
            token_path: PathBuf::from("/etc/virt-man/token"),
            cert_dir: PathBuf::from("/etc/virt-man/certs"),
            tls: TlsConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("VIRT_MAN_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/etc/virt-man/daemon.toml"));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))
        } else {
            Ok(Config::default())
        }
    }

    pub fn unix_socket_path(&self) -> PathBuf {
        if let Some(p) = &self.unix_socket {
            return PathBuf::from(p);
        }
        if unsafe { libc::geteuid() } == 0 {
            PathBuf::from("/run/virt-man/daemon.sock")
        } else {
            std::env::var("XDG_RUNTIME_DIR")
                .map(|d| PathBuf::from(d).join("virt-man/daemon.sock"))
                .unwrap_or_else(|_| PathBuf::from("/tmp/virt-man-daemon.sock"))
        }
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
