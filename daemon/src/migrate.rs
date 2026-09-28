//! Rename existing guests, networks and pools so their libvirt name is their
//! UUID, keeping the old name as their display name.
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
//! - A **network** is renamed only when nothing running uses it: renaming means
//!   undefining it, which destroys its bridge. Guests that reference it by
//!   name are rewritten to the new name first (shut-off ones only, for the
//!   same reason), and it waits until they all can be.
//! - A **pool** is renamed only when no guest references it by pool name
//!   (`<source pool=...>`); guests given disks by path, as the app does, are
//!   unaffected by a pool's name.
//! - The daemon's own records keyed by name follow: IPAM networks and firewall
//!   rules for networks and guests.
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

/// A network: rename only when every guest that uses it is shut off, since
/// renaming destroys its bridge and each user must be rewritten to the new
/// name. `users` is (guest name, shut off) for guests referencing it.
pub fn plan_network(name: &str, uuid: &str, users: &[(String, bool)]) -> Plan {
    if name == uuid {
        return Plan::Done;
    }
    let running: Vec<&str> = users
        .iter()
        .filter(|(_, off)| !off)
        .map(|(n, _)| n.as_str())
        .collect();
    if running.is_empty() {
        Plan::Rename
    } else {
        Plan::Wait(format!(
            "in use by running guest(s) {}; renamed once they are shut off",
            running.join(", ")
        ))
    }
}

/// A pool: rename only when no guest names it (a `<source pool=...>` disk
/// reference). Such references are rare -- the app gives disks by path -- and
/// rewriting them is not worth the risk of a guest that will not boot.
pub fn plan_pool(name: &str, uuid: &str, referenced_by: &[String]) -> Plan {
    if name == uuid {
        Plan::Done
    } else if referenced_by.is_empty() {
        Plan::Rename
    } else {
        Plan::Wait(format!(
            "referenced by name from guest(s) {}; left under its name",
            referenced_by.join(", ")
        ))
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

/// Point a guest's `<source network='old'/>` at `new`. Returns the rewritten
/// XML only if something changed.
pub fn repoint_network(xml: &str, old: &str, new: &str) -> Option<String> {
    let mut changed = false;
    let mut out = xml.to_string();
    for q in ['\'', '"'] {
        let from = format!("network={q}{}{q}", escape_attr(old));
        if out.contains(&from) {
            out = out.replace(&from, &format!("network={q}{}{q}", escape_attr(new)));
            changed = true;
        }
    }
    changed.then_some(out)
}

/// Whether a guest's XML references a network or a pool by name.
pub fn uses_network(xml: &str, net: &str) -> bool {
    ['\'', '"']
        .iter()
        .any(|q| xml.contains(&format!("network={q}{}{q}", escape_attr(net))))
}
pub fn uses_pool(xml: &str, pool: &str) -> bool {
    ['\'', '"']
        .iter()
        .any(|q| xml.contains(&format!("pool={q}{}{q}", escape_attr(pool))))
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
fn escape_attr(s: &str) -> String {
    escape(s).replace('\'', "&apos;").replace('"', "&quot;")
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
    fn a_network_waits_for_every_running_user() {
        let users = [("a".to_string(), true), ("b".to_string(), false)];
        match plan_network("lan", U, &users) {
            Plan::Wait(why) => assert!(why.contains('b') && !why.contains("a,"), "{why}"),
            p => panic!("{p:?}"),
        }
        assert_eq!(plan_network("lan", U, &[("a".into(), true)]), Plan::Rename);
        assert_eq!(plan_network("lan", U, &[]), Plan::Rename);
        assert_eq!(plan_network(U, U, &users), Plan::Done);
    }

    #[test]
    fn a_pool_named_by_a_guest_is_left_alone() {
        assert_eq!(plan_pool("default", U, &[]), Plan::Rename);
        assert!(matches!(
            plan_pool("default", U, &["alpine".into()]),
            Plan::Wait(_)
        ));
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
    fn guests_are_repointed_at_the_renamed_network_and_nothing_else() {
        let x = "<interface type='network'><source network='internet'/></interface>\
                 <interface type=\"network\"><source network=\"internet\"/></interface>\
                 <interface type='network'><source network='internet2'/></interface>";
        let got = repoint_network(x, "internet", U).unwrap();
        assert_eq!(got.matches(U).count(), 2, "both quote styles: {got}");
        assert!(
            got.contains("network='internet2'"),
            "a longer name is not touched: {got}"
        );
        assert!(
            repoint_network(x, "lan", U).is_none(),
            "no change, no rewrite"
        );
        assert!(uses_network(x, "internet") && !uses_network(x, "inter"));
    }

    #[test]
    fn pool_references_are_found_by_exact_name() {
        let x = "<source pool='default' volume='a.qcow2'/>";
        assert!(uses_pool(x, "default"));
        assert!(!uses_pool(x, "def"));
        assert!(!uses_pool(
            "<source file='/var/lib/libvirt/images/default.qcow2'/>",
            "default"
        ));
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
use virt::network::Network;
use virt::storage_pool::StoragePool;

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
    Network { old: String, new: String },
    Pool { old: String, new: String },
}

/// Migrate forever: a pass now, then one every `PASS_SECS`.
pub async fn run(ipam: crate::ipam::server::Ipam) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(PASS_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match pass(&ipam, everything).await {
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
pub async fn pass(ipam: &crate::ipam::server::Ipam, only: Filter) -> Result<Vec<Renamed>, String> {
    let done = with_conn(move |c| Ok(rename_all(c, only))).await?;
    // Records keyed by name follow their resource. Done after libvirt, and
    // only for what was actually renamed, so a failed rename moves nothing.
    if !done.is_empty() {
        follow_records(ipam, &done)?;
    }
    Ok(done)
}

fn rename_all(c: &Connect, only: Filter) -> Vec<Renamed> {
    let mut done = vec![];
    let domains = c.list_all_domains(0).unwrap_or_default();
    // Guest definitions, read once: network and pool plans look at them.
    let guest_xml = |c: &Connect| -> Vec<(String, bool, String)> {
        c.list_all_domains(0)
            .unwrap_or_default()
            .iter()
            .filter_map(|d| {
                let name = d.get_name().ok()?;
                let off = !d.is_active().unwrap_or(true);
                let xml = d.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE).ok()?;
                Some((name, off, xml))
            })
            .collect()
    };

    // Guests first: a guest renamed while shut off does not hold a network up.
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

    for n in c.list_all_networks(0).unwrap_or_default() {
        let (Ok(name), Ok(uuid)) = (n.get_name(), n.get_uuid_string()) else {
            continue;
        };
        if !only(&name) {
            continue;
        }
        let guests = guest_xml(c);
        let users: Vec<(String, bool)> = guests
            .iter()
            .filter(|(_, _, x)| uses_network(x, &name))
            .map(|(g, off, _)| (g.clone(), *off))
            .collect();
        match plan_network(&name, &uuid, &users) {
            Plan::Done => {}
            Plan::Wait(why) => tracing::info!("migrate: network {name}: {why}"),
            Plan::Rename => match rename_network(c, &n, &name, &uuid, &guests) {
                Ok(()) => done.push(Renamed::Network {
                    old: name,
                    new: uuid,
                }),
                Err(e) => tracing::warn!("migrate: network {name}: {e}"),
            },
        }
    }

    for p in c.list_all_storage_pools(0).unwrap_or_default() {
        let (Ok(name), Ok(uuid)) = (p.get_name(), p.get_uuid_string()) else {
            continue;
        };
        if !only(&name) {
            continue;
        }
        let refs: Vec<String> = guest_xml(c)
            .into_iter()
            .filter(|(_, _, x)| uses_pool(x, &name))
            .map(|(g, _, _)| g)
            .collect();
        match plan_pool(&name, &uuid, &refs) {
            Plan::Done => {}
            Plan::Wait(why) => tracing::info!("migrate: pool {name}: {why}"),
            Plan::Rename => match rename_pool(c, &p, &name, &uuid) {
                Ok(()) => done.push(Renamed::Pool {
                    old: name,
                    new: uuid,
                }),
                Err(e) => tracing::warn!("migrate: pool {name}: {e}"),
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

/// A network no running guest uses: redefine it under its UUID (same UUID,
/// old name as title), point its shut-off users at the new name, and start
/// it again if it was running. A failure puts the old definition back.
fn rename_network(
    c: &Connect,
    n: &Network,
    old: &str,
    uuid: &str,
    guests: &[(String, bool, String)],
) -> Result<(), String> {
    let xml = n
        .get_xml_desc(virt::sys::VIR_NETWORK_XML_INACTIVE)
        .map_err(|e| e.to_string())?;
    let next = renamed_xml(&xml, old, uuid).ok_or("no <name> in the definition")?;
    let users: Vec<&(String, bool, String)> = guests
        .iter()
        .filter(|(_, _, x)| uses_network(x, old))
        .collect();
    // Re-checked against libvirt, not the snapshot.
    for (g, _, _) in &users {
        if crate::libvirt::domain(c, g).is_ok_and(|d| d.is_active().unwrap_or(true)) {
            return Err(format!("guest {g} started; next pass"));
        }
    }
    let was_active = n.is_active().unwrap_or(false);
    let autostart = n.get_autostart().unwrap_or(false);
    if was_active {
        n.destroy().map_err(|e| format!("stop: {e}"))?;
    }
    n.undefine().map_err(|e| {
        if was_active {
            let _ = n.create();
        }
        format!("undefine: {e}")
    })?;
    let restore = |why: String| -> String {
        if let Ok(back) = Network::define_xml(c, &xml) {
            let _ = back.set_autostart(autostart);
            if was_active {
                let _ = back.create();
            }
        }
        why
    };
    let new = Network::define_xml(c, &next).map_err(|e| restore(format!("define: {e}")))?;
    // Guests follow. A guest that cannot be rewritten puts everything back:
    // leaving it pointing at a name that no longer exists would stop it
    // booting.
    let mut rewritten: Vec<(String, String)> = vec![];
    for (g, _, gx) in users {
        let Some(nx) = repoint_network(gx, old, uuid) else {
            continue;
        };
        if let Err(e) = Domain::define_xml(c, &nx) {
            for (_, orig) in &rewritten {
                let _ = Domain::define_xml(c, orig);
            }
            let _ = new.undefine();
            return Err(restore(format!("guest {g}: {e}")));
        }
        rewritten.push((g.clone(), gx.clone()));
    }
    let _ = new.set_autostart(autostart);
    if was_active {
        new.create()
            .map_err(|e| format!("renamed, but starting it failed: {e}"))?;
        crate::api::networks::reattach_taps(c, &new);
    }
    Ok(())
}

/// A pool no guest names: redefine it under its UUID, keeping it running.
/// Its display name goes to `titles`, since a pool has no `<title>`.
fn rename_pool(c: &Connect, p: &StoragePool, old: &str, uuid: &str) -> Result<(), String> {
    let xml = p
        .get_xml_desc(virt::sys::VIR_STORAGE_XML_INACTIVE)
        .map_err(|e| e.to_string())?;
    let start = xml.find("<name>").ok_or("no <name> in the definition")?;
    let end = start + xml[start..].find("</name>").ok_or("no </name>")? + "</name>".len();
    let next = format!(
        "{}<name>{}</name>{}",
        &xml[..start],
        escape(uuid),
        &xml[end..]
    );
    let was_active = p.is_active().unwrap_or(false);
    let autostart = p.get_autostart().unwrap_or(false);
    if crate::titles::get(uuid).is_none() {
        crate::titles::set(uuid, old)?;
    }
    if was_active {
        p.destroy().map_err(|e| format!("stop: {e}"))?;
    }
    p.undefine().map_err(|e| {
        if was_active {
            let _ = p.create(0);
        }
        format!("undefine: {e}")
    })?;
    let restore = |why: String| -> String {
        if let Ok(back) = StoragePool::define_xml(c, &xml, 0) {
            let _ = back.set_autostart(autostart);
            if was_active {
                let _ = back.create(0);
            }
        }
        why
    };
    let new = StoragePool::define_xml(c, &next, 0).map_err(|e| restore(format!("define: {e}")))?;
    let _ = new.set_autostart(autostart);
    if was_active {
        new.create(0)
            .map_err(|e| format!("renamed, but starting it failed: {e}"))?;
    }
    Ok(())
}

/// Move the daemon's name-keyed records to the new names: IPAM networks, and
/// firewall rules for networks and guests.
fn follow_records(ipam: &crate::ipam::server::Ipam, done: &[Renamed]) -> Result<(), String> {
    {
        let mut s = ipam.lock();
        let mut changed = false;
        for r in done {
            if let Renamed::Network { old, new } = r {
                let mut m: HashMap<String, _> =
                    std::mem::take(&mut s.networks).into_iter().collect();
                changed |= rekey(&mut m, old, new);
                s.networks = m.into_iter().collect();
            }
        }
        if changed {
            ipam.save(&s)?;
        }
    }
    let path = std::path::Path::new(crate::firewall::FIREWALL_PATH);
    let mut fw = crate::firewall::load(path)?;
    let mut changed = false;
    for r in done {
        match r {
            Renamed::Network { old, new } => changed |= rekey(&mut fw.networks, old, new),
            Renamed::Domain { old, new } => changed |= rekey(&mut fw.vms, old, new),
            Renamed::Pool { .. } => {}
        }
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
    async fn migrates_a_guest_its_network_and_a_pool() {
        const NV: &str = "/var/lib/libvirt/qemu/nvram/migprobe-vm_VARS.fd";
        let dir = "/tmp/migprobe-pool";
        std::fs::create_dir_all(dir).unwrap();
        let (vm_u, net_u, pool_u) = with_conn(move |c| {
            let net = Network::define_xml(c, "<network><name>migprobe-net</name></network>")?;
            let pool = StoragePool::define_xml(
                c,
                &format!("<pool type='dir'><name>migprobe-pool</name><target><path>{dir}</path></target></pool>"),
                0,
            )?;
            let vm = Domain::define_xml(
                c,
                &format!(
                    "<domain type='kvm'><name>migprobe-vm</name><memory unit='KiB'>131072</memory><vcpu>1</vcpu>\\
                     <os><type arch='x86_64' machine='q35'>hvm</type>\\
                     <loader readonly='yes' type='pflash' format='raw'>/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>\\
                     <nvram template='/usr/share/edk2/x64/OVMF_VARS.4m.fd' format='raw'>{NV}</nvram></os>\\
                     <features><acpi/></features><devices>\\
                     <interface type='network'><source network='migprobe-net'/><model type='virtio'/></interface>\\
                     </devices></domain>"
                ),
            )?;
            Ok((vm.get_uuid_string()?, net.get_uuid_string()?, pool.get_uuid_string()?))
        })
        .await
        .expect("define probes");
        std::fs::write(NV, b"probe-vars").unwrap();

        // The daemon's own records, keyed by the old names, in a scratch IPAM.
        let scratch = std::env::temp_dir().join("migprobe-ipam.json");
        let _ = std::fs::remove_file(&scratch);
        let ipam = crate::ipam::server::Ipam::load(scratch.clone()).unwrap();

        let first = pass(&ipam, only_probes).await;
        let second = pass(&ipam, only_probes).await;

        let (v2, n2, p2) = (vm_u.clone(), net_u.clone(), pool_u.clone());
        let after = with_conn(move |c| {
            let vm = crate::libvirt::domain(c, &v2)?;
            let net = crate::libvirt::network(c, &n2)?;
            let pool = crate::libvirt::pool(c, &p2)?;
            Ok((
                vm.get_name()?,
                vm.get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)?,
                net.get_name()?,
                net.get_xml_desc(0)?,
                pool.get_name()?,
            ))
        })
        .await;
        let new_nv = format!("/var/lib/libvirt/qemu/nvram/{vm_u}_VARS.fd");
        let vars = std::fs::read(&new_nv).ok();
        let old_nv_left = std::path::Path::new(NV).exists();
        let pool_title = crate::titles::get(&pool_u);

        // Clean up whatever state it ended in, before asserting.
        let (v3, n3, p3) = (vm_u.clone(), net_u.clone(), pool_u.clone());
        let _ = with_conn(move |c| {
            if let Ok(d) = crate::libvirt::domain(c, &v3) {
                let _ = d.undefine_flags(virt::sys::VIR_DOMAIN_UNDEFINE_NVRAM);
            }
            if let Ok(n) = crate::libvirt::network(c, &n3) {
                let _ = n.undefine();
            }
            if let Ok(p) = crate::libvirt::pool(c, &p3) {
                let _ = p.undefine();
            }
            Ok(())
        })
        .await;
        let _ = crate::titles::set(&pool_u, "");
        let _ = std::fs::remove_file(NV);
        let _ = std::fs::remove_file(&new_nv);
        let _ = std::fs::remove_dir(dir);
        let _ = std::fs::remove_file(&scratch);

        let first = first.expect("first pass");
        assert_eq!(first.len(), 3, "all three renamed: {first:?}");
        assert!(
            second.expect("second pass").is_empty(),
            "a second pass is a no-op"
        );
        let (vm_name, vm_xml, net_name, net_xml, pool_name) = after.expect("found by UUID");
        assert_eq!(vm_name, vm_u, "guest named by its UUID");
        assert!(
            vm_xml.contains("<title>migprobe-vm</title>"),
            "old name kept as title:\n{vm_xml}"
        );
        assert!(
            vm_xml.contains(&format!("network='{net_u}'")),
            "guest points at the renamed network:\n{vm_xml}"
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
        assert_eq!(net_name, net_u);
        assert!(net_xml.contains("<title>migprobe-net</title>"), "{net_xml}");
        assert_eq!(pool_name, pool_u);
        assert_eq!(
            pool_title.as_deref(),
            Some("migprobe-pool"),
            "pool title kept"
        );
    }
}
