//! Same-port HTTP→HTTPS redirect.
//!
//! Browsing to `http://host:8006` used to hand the client a raw TLS alert
//! record (`15 03 03 ...`, a `decode_error`), which curl prints as nothing and
//! a browser reports as a protocol error. Rather than run a second listener on
//! another port, peek at the first byte of every connection: a TLS handshake
//! always starts with 0x16 (ContentType::Handshake), and no HTTP method does.
//! Plaintext gets a 308 to the https:// URL; everything else proceeds to TLS.

use std::io;
use std::pin::Pin;

use axum_server::accept::Accept;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// TLS record type for a handshake; the first byte of every TLS connection.
const TLS_HANDSHAKE: u8 = 0x16;

/// Wraps an inner acceptor (the TLS one) and short-circuits plaintext HTTP.
#[derive(Clone)]
pub struct RedirectAcceptor<A> {
    inner: A,
    /// Port to name in the redirect, so a non-default port survives.
    port: u16,
}

impl<A> RedirectAcceptor<A> {
    pub fn new(inner: A, port: u16) -> Self {
        Self { inner, port }
    }
}

impl<A, S> Accept<TcpStream, S> for RedirectAcceptor<A>
where
    A: Accept<TcpStream, S> + Clone + Send + Sync + 'static,
    A::Future: Send,
    A::Stream: Send,
    A::Service: Send,
    S: Send + 'static,
{
    type Stream = A::Stream;
    type Service = A::Service;
    type Future =
        Pin<Box<dyn std::future::Future<Output = io::Result<(A::Stream, A::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let inner = self.inner.clone();
        let port = self.port;
        Box::pin(async move {
            // peek() does not consume, so a real TLS connection is handed to
            // the TLS acceptor with its handshake bytes intact.
            let mut first = [0u8; 1];
            let n = stream.peek(&mut first).await?;
            if n == 1 && first[0] != TLS_HANDSHAKE {
                respond_redirect(stream, port).await;
                // Tell the caller this connection is done. axum-server logs
                // nothing for an Err from accept(), it just drops it.
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "redirected plaintext HTTP to HTTPS",
                ));
            }
            inner.accept(stream, service).await
        })
    }
}

/// Read the request line far enough to echo Host and path back, then write a
/// 308. Hand-rolled because the connection is not an HTTP service yet.
async fn respond_redirect(mut stream: TcpStream, port: u16) {
    use tokio::io::AsyncReadExt;

    // Bounded read: enough for a request line plus headers, never unbounded
    // from an untrusted peer.
    let mut buf = vec![0u8; 4096];
    let mut len = 0;
    loop {
        match stream.read(&mut buf[len..]).await {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == buf.len() {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf[..len]);
    let target = redirect_target(&head, port);

    let body = "Redirecting to HTTPS\n";
    let resp = format!(
        "HTTP/1.1 308 Permanent Redirect\r\n\
         Location: {target}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Build the https:// URL from a raw request head. Pure, so it is testable.
pub fn redirect_target(head: &str, port: u16) -> String {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let _method = parts.next().unwrap_or("");
    let path = match parts.next().unwrap_or("/") {
        // An absolute-form target or a missing path both fall back to root.
        p if p.starts_with('/') => p,
        _ => "/",
    };

    // Host, minus any port the client sent: the client's port is the plaintext
    // one and must not be echoed back.
    let host = lines
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("host").then(|| v.trim())
        })
        .unwrap_or("");
    let bare = host_without_port(host);

    if bare.is_empty() {
        // No usable Host (HTTP/1.0 without one): send them to the path and let
        // the browser keep the host it already dialled.
        return path.to_string();
    }
    if port == 443 {
        format!("https://{bare}{path}")
    } else {
        format!("https://{bare}:{port}{path}")
    }
}

/// Strip a trailing `:port`, leaving IPv6 literals in brackets intact.
fn host_without_port(host: &str) -> &str {
    if let Some(end) = host.rfind(']') {
        // [::1]:8006 -> [::1]
        return &host[..=end];
    }
    match host.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_an_https_url_from_the_host_header() {
        let head = "GET /vms HTTP/1.1\r\nHost: vm.example:8006\r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "https://vm.example:8006/vms");
    }

    #[test]
    fn the_clients_port_is_replaced_by_ours() {
        // Client dialled :80, but we serve TLS on :8006.
        let head = "GET / HTTP/1.1\r\nHost: vm.example\r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "https://vm.example:8006/");
    }

    #[test]
    fn port_443_is_left_implicit() {
        let head = "GET /x HTTP/1.1\r\nHost: vm.example\r\n\r\n";
        assert_eq!(redirect_target(head, 443), "https://vm.example/x");
    }

    #[test]
    fn ipv6_literals_keep_their_brackets() {
        let head = "GET / HTTP/1.1\r\nHost: [::1]:8006\r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "https://[::1]:8006/");
    }

    #[test]
    fn the_host_header_is_matched_case_insensitively() {
        let head = "GET / HTTP/1.1\r\nhOsT:  vm.example  \r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "https://vm.example:8006/");
    }

    #[test]
    fn query_strings_survive() {
        let head = "GET /a?b=c&d=e HTTP/1.1\r\nHost: h\r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "https://h:8006/a?b=c&d=e");
    }

    #[test]
    fn a_missing_host_falls_back_to_the_path() {
        let head = "GET /only HTTP/1.0\r\n\r\n";
        assert_eq!(redirect_target(head, 8006), "/only");
    }

    #[test]
    fn a_junk_request_line_still_yields_root() {
        assert_eq!(
            redirect_target("\r\nHost: h\r\n\r\n", 8006),
            "https://h:8006/"
        );
    }

    #[test]
    fn a_tls_handshake_byte_is_not_mistaken_for_http() {
        // Guards the discriminator itself: no HTTP method starts with 0x16.
        for method in ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            assert_ne!(method.as_bytes()[0], TLS_HANDSHAKE, "{method}");
        }
    }
}
