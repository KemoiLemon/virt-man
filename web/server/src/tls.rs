use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// This host's own non-loopback IPv4 addresses and hostname, for the SANs.
///
/// Enumerated from the kernel, not from a CLI. `hostname` is *not installed on
/// every host* (measured: absent on this one), so shelling out to it silently
/// yields no addresses, and the certificate quietly goes back to loopback-only
/// -- the very bug this exists to fix. The daemon's installer has the same
/// trap and carries an `ip -4 -o addr` fallback for it; reading the interface
/// addresses directly has neither problem.
///
/// Best effort throughout: anything unreadable is simply left out, since a
/// missing SAN is a browser warning rather than a server that will not start.
fn host_names() -> Vec<String> {
    let mut out = Vec::new();
    // /sys/class/net lists every interface; each one's IPv4 addresses are in
    // its `address`-adjacent info. `getifaddrs` is the portable spelling, but
    // it needs libc and this is the same information for less.
    for ip in interface_addresses() {
        out.push(ip);
    }
    if let Some(name) = hostname() {
        out.push(name);
    }
    // sort + dedup, not dedup alone: `hostname -I` can repeat an address (a
    // host with the same IP on two interfaces), and dedup only removes
    // *adjacent* duplicates.
    out.sort();
    out.dedup();
    out
}

/// Every non-loopback IPv4 address on this host, via `getifaddrs`.
///
/// Read through libc rather than `/proc` or a command: it is the one source
/// that reports exactly the addresses a client could dial, including a `wg0`
/// or a bond that has no `/sys` address file, and it never depends on a tool
/// being installed.
fn interface_addresses() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs are the documented pair; we walk the list
    // it hands back and free it before returning, and copy every string we keep
    // out of the C memory rather than holding a pointer into it.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family == libc::AF_INET as u16 {
                let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                // s_addr is big-endian network order; Ipv4Addr::from takes it
                // in that order, so no byte swap.
                let ip = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                if !ip.is_loopback() {
                    out.push(ip.to_string());
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// The host's own name, from `/proc/sys/kernel/hostname` rather than the
/// `hostname` command, which may not exist.
fn hostname() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = name.trim();
    (!name.is_empty() && name != "localhost").then(|| name.to_string())
}

/// Generate a self-signed certificate on first run, key mode 0600.
///
/// The SANs include this host's own addresses, not just loopback. Without them
/// a browser reaching the UI at `https://<lan-ip>:8006` -- which is the normal
/// way to use it, since the whole point is a remote web UI -- gets a
/// certificate-name mismatch. Clicking through it leaves a page whose TLS the
/// browser does not trust, so the `Secure` session cookie is not kept and every
/// reload lands back on the sign-in screen: a page that renders but never
/// actually logs in. The daemon's installer has always added these SANs
/// (`daemon/install.sh`); the web server had none, so it only ever worked on
/// localhost.
pub fn ensure_cert(cert_path: &Path, key_path: &Path) -> Result<(), String> {
    if cert_path.exists() && key_path.exists() {
        return Ok(());
    }

    // ::1 belongs here alongside 127.0.0.1: the listener is dual-stack, so a
    // client may legitimately dial the IPv6 loopback literal, and a missing SAN
    // makes that a certificate-name failure rather than a trust prompt.
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    names.extend(host_names());
    let key = rcgen::generate_simple_self_signed(names)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated certificate must name this host, not just loopback.
    ///
    /// A UI reached at `https://<lan-ip>:8006` is the normal case -- it is a
    /// *remote* web UI -- and a SAN list of only localhost/127.0.0.1/::1 makes
    /// that a certificate-name mismatch. The browser then refuses to keep the
    /// `Secure` session cookie, so the page renders but never logs in, and
    /// reloading repeats it. Measured before the fix: `IP 192.168.0.119 does
    /// NOT match certificate`.
    #[test]
    fn the_certificate_names_every_loopback_and_this_host() {
        let names = {
            let mut v = vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ];
            v.extend(host_names());
            v
        };
        for must in ["localhost", "127.0.0.1", "::1"] {
            assert!(names.iter().any(|n| n == must), "missing {must}: {names:?}");
        }
        // Whatever this host's own addresses are, they must be in the list, so
        // the same `hostname -I` the daemon's installer uses is covered.
        if let Ok(o) = std::process::Command::new("hostname").arg("-I").output() {
            for ip in String::from_utf8_lossy(&o.stdout).split_whitespace() {
                if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                    assert!(
                        names.iter().any(|n| n == ip),
                        "the host's own address {ip} must be a SAN: {names:?}"
                    );
                }
            }
        }
    }

    /// A repeated address must appear once; `dedup` alone would miss a
    /// non-adjacent repeat, and a duplicate SAN is a wasted entry in a list a
    /// person may have to read through.
    #[test]
    fn host_names_are_unique() {
        let names = host_names();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted and deduplicated: {names:?}");
        // No v6 leaks in as a bare string: rcgen would read it as a DNS name.
        for n in &names {
            assert!(
                n.parse::<std::net::Ipv4Addr>().is_ok() || n.parse::<std::net::Ipv6Addr>().is_err(),
                "{n} looks like an address that is not IPv4"
            );
        }
    }
}
