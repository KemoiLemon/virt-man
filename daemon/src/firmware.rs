//! OVMF firmware paths that differ by distribution.
//!
//! The guest forms write the loader and NVRAM-template paths for the host they
//! were written on, and edk2 installs them under a different directory on
//! Debian and Alpine (`/usr/share/OVMF/...`) than on Arch
//! (`/usr/share/edk2/x64/...`). A definition naming the other distribution's
//! path is defined fine and then refused by qemu at start ("Could not open
//! .../OVMF_CODE.4m.fd: No such file or directory").
//!
//! The daemon runs on the hypervisor, so it is the only layer that can see the
//! filesystem. It rewrites a *known-template* path that does not exist here to
//! one that does, at define time. A path that exists is left exactly as
//! written, and an unknown path is never touched: it is not ours to guess, and
//! an administrator may deliberately name a custom build.

use std::path::Path;

/// OVMF firmware-code files, in preference order. These mirror the web
/// server's allowlist (`web/server/src/xmlcheck.rs` `FIRMWARE`).
const LOADERS: [&str; 3] = [
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd", // Arch
    "/usr/share/OVMF/OVMF_CODE_4M.fd",     // Debian
    "/usr/share/OVMF/OVMF_CODE.fd",        // Debian/Alpine, non-4M
];
/// The matching variable-store templates. A different set from `LOADERS` on
/// purpose: the two are chosen independently, so a host that has only one of a
/// pair still gets a valid pairing.
const VARS: [&str; 3] = [
    "/usr/share/edk2/x64/OVMF_VARS.4m.fd", // Arch
    "/usr/share/OVMF/OVMF_VARS_4M.fd",     // Debian
    "/usr/share/OVMF/OVMF_VARS.fd",        // Debian/Alpine
];

fn first_existing(cands: &[&'static str]) -> Option<&'static str> {
    cands.iter().copied().find(|p| Path::new(p).exists())
}

/// Replace every known-template path in `xml` that does not exist on this host
/// with the first candidate that does.
///
/// A plain substitution, not a parse: the candidate paths occur only as the
/// `<loader>` text and the `<nvram template=...>` attribute, and both are
/// fixed strings, so replacing the string reaches both. It also leaves a path
/// an administrator wrote by hand alone, because it is not one of ours.
pub fn resolve(xml: &str) -> String {
    let mut out = xml.to_string();
    for cands in [&LOADERS[..], &VARS[..]] {
        let Some(best) = first_existing(cands) else {
            continue;
        };
        for c in cands {
            if *c != best && !Path::new(c).exists() && out.contains(c) {
                out = out.replace(c, best);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_that_exists_on_this_host_is_left_as_written() {
        let Some(best) = first_existing(&LOADERS) else {
            return; // no OVMF installed here
        };
        let xml = format!("<loader readonly='yes'>{best}</loader>");
        assert_eq!(resolve(&xml), xml);
    }

    #[test]
    fn a_candidate_that_is_absent_here_is_swapped_for_one_that_exists() {
        let Some(best) = first_existing(&LOADERS) else {
            return;
        };
        let Some(absent) = LOADERS.iter().find(|p| !Path::new(p).exists()) else {
            return; // this host happens to have every candidate
        };
        let xml = format!("<loader readonly='yes'>{absent}</loader>");
        assert_eq!(
            resolve(&xml),
            format!("<loader readonly='yes'>{best}</loader>")
        );
    }

    #[test]
    fn the_nvram_template_is_resolved_independently() {
        let Some(best) = first_existing(&VARS) else {
            return;
        };
        let Some(absent) = VARS.iter().find(|p| !Path::new(p).exists()) else {
            return;
        };
        let xml = format!(
            "<nvram template='{absent}' format='raw'>/var/lib/libvirt/qemu/nvram/web_VARS.fd</nvram>"
        );
        let out = resolve(&xml);
        assert!(out.contains(best), "{out}");
        assert!(!out.contains(absent), "{out}");
        // The guest's own variable store is not a template: untouched.
        assert!(
            out.contains("/var/lib/libvirt/qemu/nvram/web_VARS.fd"),
            "{out}"
        );
    }

    #[test]
    fn a_path_that_is_not_one_of_ours_is_never_touched() {
        let xml = "<loader readonly='yes'>/opt/fw/OVMF_CODE.fd</loader>";
        assert_eq!(resolve(xml), xml);
    }

    #[test]
    fn xml_without_firmware_is_unchanged() {
        let xml = "<domain><devices><disk/></devices></domain>";
        assert_eq!(resolve(xml), xml);
    }
}
