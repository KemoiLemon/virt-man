//! Rename existing guests so their libvirt name is their UUID, keeping the old
//! name as their display name.
//!
//! Runs by itself: at startup and then on a timer, so updating the daemon on
//! any server migrates that server with no manual step. Each pass is a no-op
//! once everything is migrated.
//!
//! It is conservative on purpose, because a rename is the one thing here that
//! can cost someone a guest:
//!
//! - A **guest** is renamed only while shut off (libvirt's rule, and the only
//!   safe moment); a running one is retried on a later pass. Its UEFI variable
//!   store is renamed with it, or it would boot with fresh variables.
//! - Firewall rules for guests follow their new name.
//!
//! Storage pools are not migrated: the daemon owns them now, names them by
//! their UUID when they are created, and keeps their metadata in `pools.json`.
//! Networks are not migrated either: they are already keyed by UUID.
//!
//! Every step is re-checked against libvirt, not a cached view, and any
//! failure leaves the resource under its old name for the next pass.

use std::collections::HashMap;

/// What one resource needs, decided from what libvirt reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Already named by its UUID.
    Done,
    /// Rename now.
    Rename,
    /// Not yet: the reason is logged, and it is tried again next pass.
    Wait(String),
}

/// A guest: rename only when shut off.
pub fn plan_domain(name: &str, uuid: &str, shut_off: bool) -> Plan {
    if name == uuid {
        Plan::Done
    } else if !shut_off {
        Plan::Wait("running; renamed once it is shut off".into())
    } else {
        Plan::Rename
    }
}

/// Swap a definition's `<name>` for `new`, and give it `<title>old</title>`
/// if it has no title yet. None if the XML has no `<name>`.
///
/// The title keeps the name people chose: the libvirt name becomes a UUID,
/// which nobody wants to read. A title that is already set wins -- it is
/// what the user asked to be called.
pub fn renamed_xml(xml: &str, old: &str, new: &str) -> Option<String> {
    let start = xml.find("<name>")?;
    let end = start + xml[start..].find("</name>")? + "</name>".len();
    let mut out = String::with_capacity(xml.len() + old.len() + 32);
    out.push_str(&xml[..start]);
    out.push_str(&format!("<name>{}</name>", escape(new)));
    if !xml.contains("<title>") {
        out.push_str(&format!("\n  <title>{}</title>", escape(old)));
    }
    out.push_str(&xml[end..]);
    Some(out)
}

/// The guest's UEFI variable store, if it has one: the path in `<nvram>`.
pub fn nvram_path(xml: &str) -> Option<String> {
    let i = xml.find("<nvram")?;
    let open_end = i + xml[i..].find('>')?;
    if xml[..=open_end].ends_with("/>") {
        return None;
    }
    let close = open_end + xml[open_end..].find("</nvram>")?;
    let p = xml[open_end + 1..close].trim();
    (!p.is_empty()).then(|| crate::libvirt::unescape(p))
}

/// Rename a guest's variable store to follow its new name, in the XML: the
/// file name libvirt derives is `<name>_VARS.fd`, and keeping the old one
/// would work but read as someone else's. Returns (new XML, old path, new
/// path) when there is a store named after the old name.
pub fn move_nvram(xml: &str, old: &str, new: &str) -> Option<(String, String, String)> {
    let path = nvram_path(xml)?;
    let file = std::path::Path::new(&path).file_name()?.to_str()?;
    let want = format!("{old}_VARS.fd");
    if file != want {
        return None;
    }
    let new_path = path.strip_suffix(&want)?.to_string() + &format!("{new}_VARS.fd");
    let out = xml.replacen(
        &format!(">{}<", escape(&path)),
        &format!(">{}<", escape(&new_path)),
        1,
    );
    (out != xml).then_some((out, path, new_path))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Move a name-keyed record to its new key, in any map keyed by name. True if
/// something moved. A record already under the new key is not overwritten.
pub fn rekey<V>(map: &mut HashMap<String, V>, old: &str, new: &str) -> bool {
    if map.contains_key(new) {
        return false;
    }
    match map.remove(old) {
        Some(v) => {
            map.insert(new.to_string(), v);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "ea15249b-a713-41dc-a808-05619e42eb2a";

    #[test]
    fn a_guest_is_renamed_only_when_shut_off() {
        assert_eq!(plan_domain("web", U, true), Plan::Rename);
        assert!(matches!(plan_domain("web", U, false), Plan::Wait(_)));
        assert_eq!(plan_domain(U, U, false), Plan::Done, "already migrated");
    }

    #[test]
    fn the_old_name_becomes_the_title_unless_one_is_set() {
        let x = "<domain type='kvm'>\n  <name>Debian-13</name>\n  <uuid>u</uuid>\n</domain>";
        let got = renamed_xml(x, "Debian-13", U).unwrap();
        assert!(got.contains(&format!("<name>{U}</name>")), "{got}");
        assert!(got.contains("<title>Debian-13</title>"), "{got}");
        assert!(!got.contains("<name>Debian-13</name>"));
        // A title the user set wins.
        let titled = "<domain><name>x</name><title>My Box</title></domain>";
        let got = renamed_xml(titled, "x", U).unwrap();
        assert!(
            got.contains("<title>My Box</title>") && got.matches("<title>").count() == 1,
            "{got}"
        );
        // Names needing escaping round-trip.
        let got = renamed_xml("<domain><name>a&amp;b</name></domain>", "a&b", U).unwrap();
        assert!(got.contains("<title>a&amp;b</title>"), "{got}");
        assert!(renamed_xml("<domain/>", "x", U).is_none());
    }

    #[test]
    fn the_uefi_variable_store_follows_the_rename() {
        let x = "<os><nvram template='/t.fd' format='raw'>/var/lib/libvirt/qemu/nvram/Debian-13_VARS.fd</nvram></os>";
        let (xml, from, to) = move_nvram(x, "Debian-13", U).unwrap();
        assert_eq!(from, "/var/lib/libvirt/qemu/nvram/Debian-13_VARS.fd");
        assert_eq!(to, format!("/var/lib/libvirt/qemu/nvram/{U}_VARS.fd"));
        assert!(xml.contains(&to) && !xml.contains(&from), "{xml}");
        // A store not named after the guest is left where it is.
        let other = "<os><nvram>/var/lib/libvirt/qemu/nvram/shared_VARS.fd</nvram></os>";
        assert!(move_nvram(other, "Debian-13", U).is_none());
        // No store at all (BIOS), or a self-closing one.
        assert!(move_nvram("<os/>", "x", U).is_none());
        assert!(move_nvram("<os><nvram template='/t'/></os>", "x", U).is_none());
    }

    #[test]
    fn records_move_to_the_new_key_without_clobbering() {
        let mut m: HashMap<String, u8> = [("internet".to_string(), 1)].into();
        assert!(rekey(&mut m, "internet", U));
        assert_eq!(m.get(U), Some(&1));
        assert!(!m.contains_key("internet"));
        assert!(!rekey(&mut m, "internet", U), "nothing left to move");
        let mut clash: HashMap<String, u8> = [("a".to_string(), 1), (U.to_string(), 2)].into();
        assert!(!rekey(&mut clash, "a", U), "never overwrite");
        assert_eq!(clash.get(U), Some(&2));
    }
}

// ------------------------------------------------------------------ executor

use crate::libvirt::with_conn;
use virt::connect::Connect;
use virt::domain::Domain;

/// Seconds between passes. A guest that was running is renamed within this
/// long of being shut off.
pub const PASS_SECS: u64 = 60;

/// Only resources whose current name satisfies this are touched. Always true
/// in production; the live tests narrow it to their own throwaway resources,
/// so a test can never rename something on the host it runs on.
pub type Filter = fn(&str) -> bool;

fn everything(_: &str) -> bool {
    true
}

/// One renamed resource, for the records keyed by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Renamed {
    Domain { old: String, new: String },
}

/// Limit the pass to names starting with `VIRT_MAN_MIGRATE_ONLY`, when set.
/// A test aid: it lets the real daemon migrate throwaway resources on a host
/// that has others. Unset, everything is migrated.
fn only_prefixed(name: &str) -> bool {
    std::env::var("VIRT_MAN_MIGRATE_ONLY").map_or(true, |p| name.starts_with(&p))
}

/// Migrate forever: a pass now, then one every `PASS_SECS`.
pub async fn run(ipam: crate::ipam::server::Ipam) {
    let secs = std::env::var("VIRT_MAN_MIGRATE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(PASS_SECS);
    let filter: Filter = if std::env::var("VIRT_MAN_MIGRATE_ONLY").is_ok() {
        only_prefixed
    } else {
        everything
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match pass(&ipam, filter).await {
            Ok(done) if !done.is_empty() => {
                tracing::info!("migrate: renamed {} to UUID names", done.len())
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("migrate: {e}"),
        }
    }
}

/// One pass: rename what can be renamed now, move the records that follow
/// the name, and report what was renamed.
pub async fn pass(_ipam: &crate::ipam::server::Ipam, only: Filter) -> Result<Vec<Renamed>, String> {
    let done = with_conn(move |c| Ok(rename_all(c, only))).await?;
    // Records keyed by name follow their resource. Done after libvirt, and
    // only for what was actually renamed, so a failed rename moves nothing.
    if !done.is_empty() {
        follow_records(&done)?;
    }
    Ok(done)
}

fn rename_all(c: &Connect, only: Filter) -> Vec<Renamed> {
    let mut done = vec![];
    let domains = c.list_all_domains(0).unwrap_or_default();
    for d in &domains {
        let (Ok(name), Ok(uuid)) = (d.get_name(), d.get_uuid_string()) else {
            continue;
        };
        if !only(&name) {
            continue;
        }
        let off = !d.is_active().unwrap_or(true);
        match plan_domain(&name, &uuid, off) {
            Plan::Done => {}
            Plan::Wait(why) => tracing::debug!("migrate: guest {name}: {why}"),
            Plan::Rename => match rename_domain(c, d, &name, &uuid) {
                Ok(()) => done.push(Renamed::Domain {
                    old: name,
                    new: uuid,
                }),
                Err(e) => tracing::warn!("migrate: guest {name}: {e}"),
            },
        }
    }
    done
}

/// A shut-off guest: libvirt's rename (which keeps the UUID and every
/// device), then its display name, then its variable store's file name.
fn rename_domain(c: &Connect, d: &Domain, old: &str, uuid: &str) -> Result<(), String> {
    // Re-checked here, not trusted from the plan: it may have started since.
    if d.is_active().unwrap_or(true) {
        return Err("started before it could be renamed; next pass".into());
    }
    d.rename(uuid, 0).map_err(|e| format!("rename: {e}"))?;
    let d = crate::libvirt::domain(c, uuid).map_err(|e| e.to_string())?;
    let xml = d
        .get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)
        .map_err(|e| e.to_string())?;
    // The old name becomes the title (unless one is set) -- `renamed_xml`
    // with the name it already has only adds the title -- and the variable
    // store is renamed to match. One redefine for both.
    let mut next = renamed_xml(&xml, old, uuid).unwrap_or_else(|| xml.clone());
    let mut moved = None;
    if let Some((x, from, to)) = move_nvram(&next, old, uuid)
        && std::path::Path::new(&from).exists()
        && !std::path::Path::new(&to).exists()
    {
        std::fs::copy(&from, &to).map_err(|e| format!("copy variable store: {e}"))?;
        next = x;
        moved = Some((from, to));
    }
    if next != xml
        && let Err(e) = Domain::define_xml(c, &next)
    {
        if let Some((_, to)) = &moved {
            let _ = std::fs::remove_file(to);
        }
        // The rename stands (it is the part that matters); the title and
        // store name are not retried, so say so.
        return Err(format!("renamed, but setting its title failed: {e}"));
    }
    // The old store is removed only once the definition points at the copy.
    if let Some((from, _)) = moved {
        let _ = std::fs::remove_file(from);
    }
    Ok(())
}

/// Move the daemon's name-keyed records to the new names: firewall rules for
/// guests. Network definitions are the daemon's own and already keyed by UUID.
fn follow_records(done: &[Renamed]) -> Result<(), String> {
    let path = std::path::Path::new(crate::firewall::FIREWALL_PATH);
    let mut fw = crate::firewall::load(path)?;
    let mut changed = false;
    for Renamed::Domain { old, new } in done {
        changed |= rekey(&mut fw.vms, old, new);
    }
    if changed {
        crate::firewall::save(path, &fw)?;
    }
    Ok(())
}

#[cfg(test)]
mod live {
    //! The whole migration, against real libvirt, on throwaway resources only:
    //! the pass is filtered to names starting `migprobe-`, so it cannot touch
    //! anything else on the host. Nothing is started.
    //! `sudo -E cargo test --bins -- --ignored migrates_a_guest`
    use super::*;

    fn only_probes(n: &str) -> bool {
        n.starts_with("migprobe-")
    }

    #[tokio::test]
    #[ignore = "needs root and a reachable qemu:///system"]
    async fn migrates_a_guest() {
        const NV: &str = "/var/lib/libvirt/qemu/nvram/migprobe-vm_VARS.fd";
        let vm_u = with_conn(move |c| {
            let vm = Domain::define_xml(
                c,
                &format!(
                    "<domain type='kvm'><name>migprobe-vm</name><memory unit='KiB'>131072</memory><vcpu>1</vcpu>\
                     <os><type arch='x86_64' machine='q35'>hvm</type>\
                     <loader readonly='yes' type='pflash' format='raw'>/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>\
                     <nvram template='/usr/share/edk2/x64/OVMF_VARS.4m.fd' format='raw'>{NV}</nvram></os>\
                     <features><acpi/></features><devices></devices></domain>"
                ),
            )?;
            Ok(vm.get_uuid_string()?)
        })
        .await
        .expect("define probe");
        std::fs::write(NV, b"probe-vars").unwrap();

        // A scratch IPAM, so the pass never touches the host's own networks.
        let scratch = std::env::temp_dir().join("migprobe-networks.json");
        let _ = std::fs::remove_file(&scratch);
        let ipam = crate::ipam::server::Ipam::load(scratch.clone(), std::env::temp_dir()).unwrap();

        let first = pass(&ipam, only_probes).await;
        let second = pass(&ipam, only_probes).await;

        let v2 = vm_u.clone();
        let after = with_conn(move |c| {
            let vm = crate::libvirt::domain(c, &v2)?;
            Ok((
                vm.get_name()?,
                vm.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?,
            ))
        })
        .await;
        let new_nv = format!("/var/lib/libvirt/qemu/nvram/{vm_u}_VARS.fd");
        let vars = std::fs::read(&new_nv).ok();
        let old_nv_left = std::path::Path::new(NV).exists();

        // Clean up whatever state it ended in, before asserting.
        let v3 = vm_u.clone();
        let _ = with_conn(move |c| {
            if let Ok(d) = crate::libvirt::domain(c, &v3) {
                let _ = d.undefine_flags(virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM);
            }
            Ok(())
        })
        .await;
        let _ = std::fs::remove_file(NV);
        let _ = std::fs::remove_file(&new_nv);
        let _ = std::fs::remove_file(&scratch);

        let first = first.expect("first pass");
        assert_eq!(first.len(), 1, "guest renamed: {first:?}");
        assert!(
            second.expect("second pass").is_empty(),
            "a second pass is a no-op"
        );
        let (vm_name, vm_xml) = after.expect("found by UUID");
        assert_eq!(vm_name, vm_u, "guest named by its UUID");
        assert!(
            vm_xml.contains("<title>migprobe-vm</title>"),
            "old name kept as title:\n{vm_xml}"
        );
        assert!(
            vm_xml.contains(&new_nv),
            "variable store renamed with it:\n{vm_xml}"
        );
        assert_eq!(
            vars.as_deref(),
            Some(&b"probe-vars"[..]),
            "and its contents kept"
        );
        assert!(!old_nv_left, "the old store file is gone");
    }
}
