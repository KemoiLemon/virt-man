use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Generate a self-signed certificate on first run, key mode 0600.
///
/// ponytail: only regenerates when files are missing — rcgen's default
/// validity is 1975..4096, so expiry regeneration is not needed in practice.
pub fn ensure_cert(cert_path: &Path, key_path: &Path) -> Result<(), String> {
    if cert_path.exists() && key_path.exists() {
        return Ok(());
    }

    let key =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
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
