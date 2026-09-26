use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Generate a self-signed certificate on first run, key mode 0600.
/// Same policy as the daemon's TLS: files are only created when missing.
pub fn ensure_cert(cert_path: &Path, key_path: &Path) -> Result<(), String> {
    if cert_path.exists() && key_path.exists() {
        return Ok(());
    }

    // ::1 belongs here alongside 127.0.0.1: the listener is dual-stack, so a
    // client may legitimately dial the IPv6 loopback literal, and a missing SAN
    // makes that a certificate-name failure rather than a trust prompt.
    let key = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .map_err(|e| format!("failed to generate TLS certificate: {e}"))?;

    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    fs::write(cert_path, key.cert.pem())
        .map_err(|e| format!("failed to write {}: {e}", cert_path.display()))?;
    fs::set_permissions(cert_path, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("failed to chmod {}: {e}", cert_path.display()))?;

    fs::write(key_path, key.signing_key.serialize_pem())
        .map_err(|e| format!("failed to write {}: {e}", key_path.display()))?;
    fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("failed to chmod {}: {e}", key_path.display()))?;

    Ok(())
}
