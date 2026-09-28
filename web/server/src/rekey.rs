//! Follow a daemon's renames in this server's own records.
//!
//! The daemon renames each guest, network and pool to its UUID (see
//! `daemon/src/migrate.rs`), on its own schedule -- a guest only once it is
//! shut off. This server keys ownership, suspensions and ACL grants by the
//! libvirt name, so after a rename they would point at a name that no longer
//! exists: the owner would lose their guest, a share would vanish, a
//! "NoAccess on this guest" would stop applying.
//!
//! So this server remembers the UUID of each name it has seen, and when a
//! later listing shows that UUID under a different name, the records and every
//! grant on its path move to the new name. Nothing is inferred from names
//! alone: without the UUID there is no telling a rename from a delete and a
//! re-create, and the second must NOT inherit the first's owner or grants.

use crate::perm::{net_path, pool_path, vm_path, Kind};
use crate::store::Store;
use std::collections::HashMap;

/// One daemon's current guests, networks and pools, each as UUID -> name.
/// `was` holds each one's title ("vm:<uuid>" -> title): for anything the
/// daemon renamed to its UUID, the name it had (the migration keeps it there).
#[derive(Debug, Default, Clone)]
pub struct Listing {
    pub vms: HashMap<String, String>,
    pub networks: HashMap<String, String>,
    pub pools: HashMap<String, String>,
    pub was: HashMap<String, String>,
}

/// Move every grant on `from` (and below it) to `to`. Returns how many moved.
fn move_grants(store: &mut Store, from: &str, to: &str) -> usize {
    let mut n = 0;
    let below = format!("{from}/");
    for e in store.acl.iter_mut() {
        if e.path == from {
            e.path = to.to_string();
            n += 1;
        } else if let Some(rest) = e.path.strip_prefix(&below) {
            e.path = format!("{to}/{rest}");
            n += 1;
        }
    }
    n
}

/// A daemon upgraded before this server may have renamed things this server
/// never saw under their old name, so there is no remembered name to compare.
/// The migration keeps the old name as the title, so a resource *named by its
/// own UUID* whose title matches a record here is that record, renamed.
///
/// Only a UUID-named resource: its name is its identity, so a title can never
/// make an unrelated guest that merely shares a display name capture another
/// one's owner and grants. And never while something live still holds the
/// old name -- then the record is that one's.
fn adopt_early_renames(
    store: &Store,
    conn: &str,
    now: &Listing,
    known: &mut HashMap<String, String>,
) {
    for (kind, list) in [
        ("vm", &now.vms),
        ("network", &now.networks),
        ("pool", &now.pools),
    ] {
        for (uuid, name) in list {
            let key = format!("{kind}:{uuid}");
            if known.contains_key(&key) || !name.eq_ignore_ascii_case(uuid) {
                continue;
            }
            let Some(old) = now.was.get(&key) else {
                continue;
            };
            if list.values().any(|n| n == old) {
                continue;
            }
            let (rkind, path) = match kind {
                "vm" => (Some(Kind::Vm), vm_path(conn, old)),
                "network" => (Some(Kind::Network), net_path(conn, old)),
                _ => (None, pool_path(conn, old)),
            };
            let has_record = rkind.is_some_and(|k| {
                store
                    .resources
                    .iter()
                    .any(|r| r.kind == k && r.conn == conn && &r.name == old)
            });
            if has_record || store.acl.iter().any(|e| e.path == path) {
                known.insert(key, old.clone());
            }
        }
    }
}

/// Bring `store` up to date with one daemon's listing. Returns how many
/// records and grants moved. Idempotent: a second call with the same listing
/// moves nothing.
pub fn apply(store: &mut Store, conn: &str, now: &Listing) -> usize {
    let mut moved = 0;
    // What this server last knew each UUID to be called, per kind.
    let mut known = store
        .known_names
        .entry(conn.to_string())
        .or_default()
        .clone();
    adopt_early_renames(store, conn, now, &mut known);

    for (kind, list, path_of) in [
        ("vm", &now.vms, vm_path as fn(&str, &str) -> String),
        ("network", &now.networks, net_path),
        ("pool", &now.pools, pool_path),
    ] {
        for (uuid, new) in list {
            let key = format!("{kind}:{uuid}");
            let Some(old) = known.get(&key) else { continue };
            if old == new {
                continue;
            }
            // Renamed. Its records, then its grants, follow it.
            let rkind = match kind {
                "vm" => Some(Kind::Vm),
                "network" => Some(Kind::Network),
                _ => None,
            };
            if let Some(k) = rkind {
                for r in store.resources.iter_mut() {
                    if r.kind == k && r.conn == conn && &r.name == old {
                        r.name = new.clone();
                        moved += 1;
                    }
                }
            }
            moved += move_grants(store, &path_of(conn, old), &path_of(conn, new));
        }
    }

    // Remember the current names for next time. Only what is listed now: a
    // UUID that disappeared was deleted, and forgetting it means a later
    // resource can never be mistaken for it.
    let mut next = HashMap::new();
    for (kind, list) in [
        ("vm", &now.vms),
        ("network", &now.networks),
        ("pool", &now.pools),
    ] {
        for (uuid, name) in list {
            next.insert(format!("{kind}:{uuid}"), name.clone());
        }
    }
    store.known_names.insert(conn.to_string(), next);
    moved
}

/// Read one daemon's listing. None if any list fails: a partial listing
/// would make present resources look deleted, and they would be forgotten.
async fn listing(conn: &crate::store::Connection) -> Option<Listing> {
    let mut out = Listing::default();
    for (kind, path) in [
        ("vm", "/domains"),
        ("network", "/networks"),
        ("pool", "/pools"),
    ] {
        let v = crate::guard::daemon_get(conn, path).await.ok()?;
        for x in v.as_array()? {
            let (Some(u), Some(n)) = (x["uuid"].as_str(), x["name"].as_str()) else {
                // A daemon too old to report UUIDs: nothing to follow.
                return None;
            };
            let u = u.to_ascii_lowercase();
            if let Some(t) = x["title"].as_str().filter(|t| !t.is_empty()) {
                out.was.insert(format!("{kind}:{u}"), t.to_string());
            }
            let map = match kind {
                "vm" => &mut out.vms,
                "network" => &mut out.networks,
                _ => &mut out.pools,
            };
            map.insert(u, n.to_string());
        }
    }
    Some(out)
}

/// Every 30 s, follow each reachable daemon's renames.
pub async fn watch(state: crate::state::AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let conns = state.read().connections.clone();
        for c in &conns {
            let Some(now) = listing(c).await else {
                continue;
            };
            let mut store = state.write();
            if store.connection(&c.id).is_none() {
                continue; // removed while we were reading
            }
            let before = store.known_names.get(&c.id).cloned();
            let moved = apply(&mut store, &c.id, &now);
            if moved > 0 {
                tracing::info!("followed renames on {}: {moved} record(s) moved", c.name);
            }
            if moved > 0 || store.known_names.get(&c.id) != before.as_ref() {
                if let Err(e) = store.save(&state.data_dir) {
                    tracing::warn!("rekey: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::{AclEntry, Resource};

    const U: &str = "ea15249b-a713-41dc-a808-05619e42eb2a";

    fn listing(vms: &[(&str, &str)]) -> Listing {
        Listing {
            vms: vms
                .iter()
                .map(|(u, n)| (u.to_string(), n.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn store() -> Store {
        let mut s = Store::default();
        s.resources.push(Resource {
            kind: Kind::Vm,
            conn: "c".into(),
            pool: String::new(),
            name: "web".into(),
            owner: "bob".into(),
            suspended: true,
            path: String::new(),
            capacity: 0,
            blank: false,
            created_ms: 0,
        });
        for (path, ugid) in [
            (vm_path("c", "web"), "user:eve"),
            (vm_path("c", "webby"), "user:x"),
        ] {
            s.acl.push(AclEntry {
                path,
                ugid: ugid.into(),
                role: "VMUser".into(),
                propagate: false,
            });
        }
        s
    }

    #[test]
    fn a_renamed_guest_keeps_its_owner_suspension_and_shares() {
        let mut s = store();
        apply(&mut s, "c", &listing(&[(U, "web")])); // learns web = U
        let moved = apply(&mut s, "c", &listing(&[(U, U)])); // renamed to its UUID
        assert!(moved >= 2, "record and grant moved: {moved}");
        let r = &s.resources[0];
        assert_eq!(
            (r.name.as_str(), r.owner.as_str(), r.suspended),
            (U, "bob", true)
        );
        assert!(
            s.acl
                .iter()
                .any(|e| e.path == vm_path("c", U) && e.ugid == "user:eve"),
            "share followed"
        );
        assert!(
            s.acl.iter().any(|e| e.path == vm_path("c", "webby")),
            "a name sharing a prefix is untouched"
        );
        assert_eq!(apply(&mut s, "c", &listing(&[(U, U)])), 0, "idempotent");
    }

    /// Delete "web", make a new guest also called "web": it must not inherit
    /// the old one's owner or grants. Only a matching UUID is a rename.
    #[test]
    fn a_new_guest_reusing_a_name_is_not_a_rename() {
        let mut s = store();
        apply(&mut s, "c", &listing(&[(U, "web")]));
        let other = "6f4d2a3b-0c9e-4d8f-b2a1-3e4c5d6e7f80";
        let moved = apply(&mut s, "c", &listing(&[(other, "web")]));
        assert_eq!(moved, 0);
        // And once the old UUID is gone, it is forgotten: a later "rename"
        // of the new guest cannot be matched to the old one's records.
        assert!(!s.known_names["c"].contains_key(&format!("vm:{U}")));
    }

    /// The daemon was upgraded first and renamed "web" before this server
    /// ever listed it: the migration kept "web" as the title, and "web" has a
    /// record here, so it is followed.
    #[test]
    fn a_rename_that_happened_before_first_sight_is_followed_by_its_title() {
        let mut s = store();
        let mut l = listing(&[(U, U)]);
        l.was.insert(format!("vm:{U}"), "web".into());
        let moved = apply(&mut s, "c", &l);
        assert!(moved >= 2, "record and share followed: {moved}");
        assert_eq!(s.resources[0].name, U);
        assert!(s
            .acl
            .iter()
            .any(|e| e.path == vm_path("c", U) && e.ugid == "user:eve"));
    }

    /// A different guest whose display name happens to be "web" must not
    /// capture web's record: only a guest named by its own UUID can be a
    /// migrated one, and never while "web" itself is still live.
    #[test]
    fn a_title_alone_never_captures_a_record() {
        let other = "6f4d2a3b-0c9e-4d8f-b2a1-3e4c5d6e7f80";
        // "web" still live under its name; a UUID-named guest titled web.
        let mut s = store();
        let mut l = listing(&[(U, "web"), (other, other)]);
        l.was.insert(format!("vm:{other}"), "web".into());
        assert_eq!(apply(&mut s, "c", &l), 0);
        assert_eq!(s.resources[0].name, "web", "web keeps its own record");
        // A plain-named guest titled web.
        let mut s = store();
        let mut l = listing(&[(other, "notes")]);
        l.was.insert(format!("vm:{other}"), "web".into());
        assert_eq!(apply(&mut s, "c", &l), 0);
        assert_eq!(s.resources[0].name, "web");
    }

    #[test]
    fn nothing_moves_on_first_sight() {
        let mut s = store();
        assert_eq!(
            apply(&mut s, "c", &listing(&[(U, U)])),
            0,
            "no prior name to compare"
        );
        assert_eq!(s.resources[0].name, "web");
    }

    #[test]
    fn a_renamed_pool_takes_its_grants_along() {
        let mut s = Store::default();
        s.acl.push(AclEntry {
            path: pool_path("c", "default"),
            ugid: "user:bob".into(),
            role: "DatastoreUser".into(),
            propagate: true,
        });
        let l = |n: &str| Listing {
            pools: [(U.to_string(), n.to_string())].into(),
            ..Default::default()
        };
        apply(&mut s, "c", &l("default"));
        assert_eq!(apply(&mut s, "c", &l(U)), 1);
        assert_eq!(s.acl[0].path, pool_path("c", U));
    }
}
