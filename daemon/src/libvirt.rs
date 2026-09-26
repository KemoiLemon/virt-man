use virt::connect::Connect;

pub const URI: &str = "qemu:///system";

/// Error type for closures run inside [`with_conn`]. Accepts libvirt errors,
/// plain strings, and I/O errors via `?`.
#[derive(Debug)]
pub enum LvError {
    Virt(virt::error::Error),
    Msg(String),
}

impl std::fmt::Display for LvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LvError::Virt(e) => write!(f, "{e}"),
            LvError::Msg(m) => write!(f, "{m}"),
        }
    }
}

impl From<virt::error::Error> for LvError {
    fn from(e: virt::error::Error) -> Self {
        LvError::Virt(e)
    }
}

impl From<String> for LvError {
    fn from(s: String) -> Self {
        LvError::Msg(s)
    }
}

impl From<std::io::Error> for LvError {
    fn from(e: std::io::Error) -> Self {
        LvError::Msg(e.to_string())
    }
}

/// Run a closure with a fresh libvirt connection on a blocking thread.
///
/// ponytail: open a connection per operation instead of keeping a persistent
/// one + actor thread; libvirt wants each connection confined to one thread,
/// and the local socket handshake is cheap. Revisit if request volume grows.
pub async fn with_conn<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Connect) -> Result<T, LvError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut conn = Connect::open(Some(URI)).map_err(|e| format!("libvirt connect: {e}"))?;
        let out = f(&conn).map_err(|e| e.to_string());
        // `virt::Connect` has no Drop impl, so an unclosed connection leaks
        // its libvirt event fds. A polling client exhausts the process fd
        // limit after a few hundred requests, and every later call then fails
        // with "Too many open files". Close on both the ok and error paths.
        if let Err(e) = conn.close() {
            tracing::warn!("libvirt connection close failed: {e}");
        }
        out
    })
    .await
    .map_err(|e| format!("libvirt task panicked: {e}"))?
}

pub fn domain_state_name(state: virt::sys::virDomainState) -> &'static str {
    match state as i32 {
        0 => "nostate",
        1 => "running",
        2 => "blocked",
        3 => "paused",
        4 => "shutdown",
        5 => "shutoff",
        6 => "crashed",
        7 => "pmsuspended",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Graphics {
    pub kind: String,
    pub host: String,
    pub port: u16,
}

/// Extract the first `<graphics>` element from domain XML and resolve the
/// TCP endpoint clients should connect to.
pub fn parse_graphics(xml: &str) -> Option<Graphics> {
    let start = xml.find("<graphics")?;
    let end = start + xml[start..].find('>')?;
    let tag = &xml[start..=end];

    let kind = attr(tag, "type")?.to_string();
    let port: u16 = attr(tag, "port")?.parse().ok()?;
    let host = attr(tag, "listen")
        .or_else(|| nested_listen_address(&xml[start..end]))
        .unwrap_or("127.0.0.1")
        .to_string();

    Some(Graphics { kind, host, port })
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let i = tag.find(name)?;
    let rest = tag[i + name.len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = rest.chars().next()?;
    let inner = rest.strip_prefix(quote)?;
    let j = inner.find(quote)?;
    Some(&inner[..j])
}

fn nested_listen_address(block: &str) -> Option<&str> {
    let i = block.find("<listen")?;
    attr(&block[i..], "address")
}
