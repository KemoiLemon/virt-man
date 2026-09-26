use base64::Engine as _;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use tokio_rustls::TlsConnector;

use crate::store::{sha256_hex, Connection};

/// Parsed daemon endpoint.
pub struct DaemonAddr {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub base: String,
}

pub fn parse_url(url: &str) -> Result<DaemonAddr, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("invalid daemon URL: {url} (expected http(s)://host[:port])"))?;
    let tls = match scheme {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(format!("unsupported scheme: {scheme}")),
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .map_err(|_| format!("invalid port in URL: {url}"))?;
            (h.to_string(), port)
        }
        None => (hostport.to_string(), if tls { 8443 } else { 8444 }),
    };
    let base = format!("{}://{}:{}", if tls { "https" } else { "http" }, host, port);
    Ok(DaemonAddr {
        host,
        port,
        tls,
        base: format!("{base}{path}"),
    })
}

/// Fetch the daemon's TLS certificate (without verifying it) and return the
/// leaf PEM plus its SHA-256 fingerprint (over the DER, like browsers show).
pub async fn fetch_cert(url: &str) -> Result<(String, String), String> {
    let addr = parse_url(url)?;
    if !addr.tls {
        return Err("daemon URL is not https, no certificate to pin".into());
    }

    let stream = tokio::net::TcpStream::connect((addr.host.as_str(), addr.port))
        .await
        .map_err(|e| format!("connect {}:{}: {e}", addr.host, addr.port))?;

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let server_name =
        ServerName::try_from(addr.host.clone()).map_err(|e| format!("invalid host: {e}"))?;
    let connector = TlsConnector::from(Arc::new(config));
    let tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("tls handshake: {e}"))?;

    let certs = tls
        .get_ref()
        .1
        .peer_certificates()
        .ok_or_else(|| "no certificate presented".to_string())?;
    let leaf = certs
        .first()
        .ok_or_else(|| "empty certificate chain".to_string())?;
    let der = leaf.as_ref();
    let fingerprint = fingerprint_of_der(der);

    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");

    Ok((pem, fingerprint))
}

/// SHA-256 fingerprint of a DER certificate, colon-separated hex pairs.
pub fn fingerprint_of_der(der: &[u8]) -> String {
    let hex = sha256_hex(der);
    hex.as_bytes()
        .chunks(2)
        .map(|c| std::str::from_utf8(c).unwrap().to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// Parse PEM cert(s) into DER.
fn parse_certs(pem: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("parse certificate: {e}"))
}

/// Same fingerprint, but from a PEM string.
pub fn fingerprint_of_pem(pem: &str) -> Result<String, String> {
    let certs = parse_certs(pem)?;
    let leaf = certs
        .first()
        .ok_or_else(|| "no certificate in PEM".to_string())?;
    Ok(fingerprint_of_der(leaf.as_ref()))
}

/// Build a rustls `ClientConfig` that trusts ONLY the pinned certificate.
pub fn pinned_config(conn: &Connection) -> Result<ClientConfig, String> {
    let pem = conn
        .cert
        .as_deref()
        .ok_or_else(|| "connection has no pinned certificate".to_string())?;
    let certs = parse_certs(pem)?;
    let mut roots = RootCertStore::empty();
    for c in certs {
        roots
            .add(c)
            .map_err(|e| format!("add pinned certificate: {e}"))?;
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// reqwest client that talks to the daemon. For https, trusts ONLY the pinned
/// certificate; for http, plain client.
pub fn http_client(conn: &Connection) -> Result<reqwest::Client, String> {
    if conn.url.starts_with("http://") || conn.url.starts_with("ws://") {
        return reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build http client: {e}"));
    }
    let config = pinned_config(conn)?;
    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| format!("build pinned client: {e}"))
}

/// rustls verifier that accepts anything. Used only to *read* the peer cert
/// during first contact; the real connection uses the pinned cert.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}
