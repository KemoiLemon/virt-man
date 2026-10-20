use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::perm;
pub use crate::perm::{AclEntry, Group, Kind, Privs, Quota, Resource, RoleDef};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub nickname: String,
    pub avatar: Option<String>,
    /// Pre-ACL fields, read only to migrate an old store.
    #[serde(default, skip_serializing)]
    pub role: String,
    #[serde(default, skip_serializing)]
    pub scopes: HashMap<String, String>,
    #[serde(default)]
    pub quota: Quota,
    /// Set for a request made with an API key: the privileges the key may
    /// use. Never stored.
    #[serde(skip)]
    pub key_privs: Option<Privs>,
}

impl User {
    pub fn new(username: String, password_hash: String, nickname: String) -> Self {
        User {
            id: random_hex(8),
            username,
            password_hash,
            nickname,
            avatar: None,
            role: String::new(),
            scopes: HashMap::new(),
            quota: Quota::default(),
            key_privs: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub url: String,
    pub token: String,
    /// PEM of the daemon's TLS certificate, pinned on first contact.
    pub cert: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub user_id: Option<String>,
    pub name: String,
    /// sha256 hex of the key; the key itself is shown only once.
    pub hash: String,
    pub prefix: String,
    /// Privilege ceiling for this key. Empty means "whatever the owner has".
    /// A key can only ever narrow its owner's access, never widen it.
    #[serde(default)]
    pub privs: Vec<String>,
    /// Pre-ACL ceiling, read only to migrate an old store.
    #[serde(default, skip_serializing)]
    pub scopes: HashMap<String, String>,
}

/// One recorded action by one user, kept so the admin panel can show who did
/// what and when. Persisted in the store, so the log survives restarts — the
/// client-side task tracker it replaces lost everything on reload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    /// Owning account. Entries are filtered by this unless the caller is an
    /// admin, so a non-admin only ever sees their own history.
    pub user_id: String,
    /// Snapshot of the name at the time of the action: the account can be
    /// renamed or removed later, and the log should still be readable.
    pub username: String,
    pub label: String,
    /// "done" or "failed".
    pub status: String,
    #[serde(default)]
    pub message: String,
    /// Unix milliseconds, so the browser can format in local time.
    pub at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub users: Vec<User>,
    /// session token (sha256 hex) -> user id. Opaque token stored hashed.
    pub sessions: HashMap<String, String>,
    pub connections: Vec<Connection>,
    pub api_keys: Vec<ApiKey>,
    pub avatar_seq: u64,
    #[serde(default)]
    pub roles: Vec<RoleDef>,
    /// Built-in roles an administrator explicitly removed. Without this,
    /// startup seeding would recreate them on every restart.
    #[serde(default)]
    pub removed_builtin_roles: Vec<String>,
    /// Newest-last append-only action log. `#[serde(default)]` so a store
    /// written before this existed still loads.
    #[serde(default)]
    pub activity: Vec<ActivityEntry>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub acl: Vec<AclEntry>,
    /// Who owns each VM, network and volume made through this app.
    #[serde(default)]
    pub resources: Vec<Resource>,
    /// Per daemon, what each resource was last called: "vm:<uuid>" -> name.
    /// How a rename is told apart from a delete-and-recreate (see `rekey`).
    #[serde(default)]
    pub known_names: HashMap<String, HashMap<String, String>>,
    /// Bumped when the permission model changes shape, so migration runs once.
    #[serde(default)]
    pub perm_version: u32,
}

/// The permission model this build writes. 0 is the old per-scope one.
const PERM_VERSION: u32 = 1;

impl Store {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("store.json");
        let mut store: Store = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            let mut v: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
            // Containers are gone, and `Kind` no longer has a variant for
            // them: a leftover ownership row would make the whole store
            // unparseable and the server would not start.
            if let Some(rows) = v.get_mut("resources").and_then(|r| r.as_array_mut()) {
                rows.retain(|r| r.get("kind").and_then(|k| k.as_str()) != Some("ct"));
            }
            serde_json::from_value(v)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?
        } else {
            Store::default()
        };
        store.migrate();
        store.seed_builtin_roles();
        Ok(store)
    }

    /// Add missing built-in roles unless an administrator explicitly removed
    /// them. Existing definitions may have been edited by an administrator.
    pub fn seed_builtin_roles(&mut self) {
        for def in perm::builtin_roles() {
            if self.removed_builtin_roles.contains(&def.name) {
                self.roles.retain(|r| r.name != def.name);
                continue;
            }
            if !self.roles.iter().any(|r| r.name == def.name) {
                self.roles.push(def);
            }
        }
    }

    /// Turn a pre-ACL store into ACL entries, once.
    ///
    /// Every role keeps its old access as privileges (`legacy_privs`), and
    /// every user gets their role, plus a personal role for any per-user
    /// overrides, at `/`. A key's scope ceiling becomes a privilege ceiling.
    /// Nothing is owned yet, so everything already on a daemon falls to the
    /// first administrator (`default_owner`).
    fn migrate(&mut self) {
        if self.perm_version >= PERM_VERSION {
            return;
        }
        let old_roles: Vec<RoleDef> = std::mem::take(&mut self.roles);
        let privs_of = |scopes: &HashMap<String, String>| perm::legacy_privs(scopes);
        let is_old_builtin = |n: &str| matches!(n, "admin" | "operator" | "viewer");
        for r in &old_roles {
            if !is_old_builtin(&r.name) {
                self.roles.push(RoleDef {
                    name: r.name.clone(),
                    builtin: false,
                    privs: privs_of(&r.scopes),
                    scopes: HashMap::new(),
                });
            }
        }
        let legacy = |name: &str| -> HashMap<String, String> {
            match old_roles.iter().find(|r| r.name == name) {
                Some(r) if !r.scopes.is_empty() => r.scopes.clone(),
                _ => {
                    // A store from before roles were data: fall back to the
                    // historical meaning of the three names.
                    let lvl = |admin: &str, op: &str, view: &str| match name {
                        "admin" => admin.to_string(),
                        "operator" => op.to_string(),
                        "viewer" => view.to_string(),
                        _ => "none".to_string(),
                    };
                    ["connections", "vms", "networks", "storage"]
                        .iter()
                        .map(|s| (s.to_string(), lvl("write", "write", "read")))
                        .chain(
                            ["users", "settings"]
                                .iter()
                                .map(|s| (s.to_string(), lvl("write", "none", "none"))),
                        )
                        .collect()
                }
            }
        };
        let mut acl = vec![];
        let mut extra_roles = vec![];
        for u in &self.users {
            let mut scopes = legacy(&u.role);
            scopes.extend(u.scopes.clone());
            let privs = privs_of(&scopes);
            let full = perm::PRIVILEGES
                .iter()
                .all(|p| privs.iter().any(|x| x == p));
            let role = if full {
                perm::ADMIN_ROLE.to_string()
            } else if u.scopes.is_empty() && !is_old_builtin(&u.role) {
                u.role.clone()
            } else {
                let name = format!("legacy-{}", u.username);
                extra_roles.push(RoleDef {
                    name: name.clone(),
                    builtin: false,
                    privs,
                    scopes: HashMap::new(),
                });
                name
            };
            acl.push(AclEntry {
                path: "/".into(),
                ugid: format!("user:{}", u.id),
                role,
                propagate: true,
            });
        }
        self.roles.extend(extra_roles);
        self.acl.extend(acl);
        for k in &mut self.api_keys {
            if !k.scopes.is_empty() && k.privs.is_empty() {
                let mut all: HashMap<String, String> =
                    ["connections", "vms", "networks", "storage", "users"]
                        .iter()
                        .map(|s| (s.to_string(), "write".to_string()))
                        .collect();
                all.extend(k.scopes.clone());
                k.privs = privs_of(&all);
            }
        }
        self.perm_version = PERM_VERSION;
    }

    /// Atomic write: temp file + rename, so a crash mid-write cannot truncate
    /// the store.
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("create {}: {e}", data_dir.display()))?;
        let path = data_dir.join("store.json");
        let tmp = data_dir.join("store.json.tmp");
        let text = serde_json::to_vec_pretty(self).map_err(|e| format!("serialize store: {e}"))?;
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename store: {e}"))?;
        Ok(())
    }

    pub fn user_by_id(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }

    pub fn user_by_username(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|u| u.username == name)
    }

    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn role(&self, name: &str) -> Option<&RoleDef> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// Who owns a resource. Anything not recorded belongs to the first
    /// administrator: guests that predate ownership, or were made outside this
    /// app, stay administrable without being handed to every user.
    pub fn owner_of(&self, kind: Kind, conn: &str, name: &str) -> Option<String> {
        self.resource(kind, conn, name)
            .map(|r| r.owner.clone())
            .or_else(|| self.default_owner())
    }

    pub fn resource(&self, kind: Kind, conn: &str, name: &str) -> Option<&Resource> {
        self.resources
            .iter()
            .find(|r| r.kind == kind && r.conn == conn && r.name == name)
    }

    pub fn resource_mut(&mut self, kind: Kind, conn: &str, name: &str) -> Option<&mut Resource> {
        self.resources
            .iter_mut()
            .find(|r| r.kind == kind && r.conn == conn && r.name == name)
    }

    /// The ACL's answer, except for loopback mode's synthetic user, which is
    /// the whole install's administrator by definition and has no entries.
    /// Its id cannot collide with a real account (those are 16 hex digits).
    fn acl_privs(&self, user: &User, path: &str) -> Privs {
        if user.id == crate::auth::LOCAL_ID {
            return perm::PRIVILEGES.iter().map(|s| s.to_string()).collect();
        }
        perm::acl_privs(&self.acl, &self.roles, &self.groups, &user.id, path)
    }

    pub fn default_owner(&self) -> Option<String> {
        self.users
            .iter()
            .find(|u| self.is_admin(u))
            .map(|u| u.id.clone())
    }

    /// Everything `user` may do at `path`.
    ///
    /// The ACL decides, then ownership adds: the owner of a VM or network
    /// holds all of its privileges. A suspended resource keeps only the
    /// read-only ones for anyone who is not an administrator — its owner
    /// included, which is the point of suspending it. An API key then caps the
    /// result to its own list.
    pub fn privs(&self, user: &User, path: &str) -> Privs {
        let mut p = self.acl_privs(user, path);
        if let Some((kind, conn, name)) = perm::path_resource(path) {
            // `*.Allocate` means "create here" on a collection and "delete
            // this" on a resource. The default role grants the first at `/`,
            // and it propagated onto every resource as the second: any user
            // could delete anyone's VM or network. On a resource, Allocate
            // therefore counts only from a grant that is not merely the
            // inherited create -- an administrator's, or one made on the
            // resource or its daemon (a share, a VMAdmin grant) -- and from
            // ownership, below.
            if !self.is_admin_raw(user) && !self.granted_below_root(user, path) {
                p.retain(|x| !x.ends_with(".Allocate"));
            }
            if self.owner_of(kind, conn, name).as_deref() == Some(user.id.as_str()) {
                p.extend(perm::owner_privs(kind).iter().map(|s| s.to_string()));
            }
            let suspended = self.resource(kind, conn, name).is_some_and(|r| r.suspended);
            if suspended && !self.is_admin_raw(user) {
                p.retain(|x| x.ends_with(".Audit"));
            }
        }
        if let Some(cap) = &user.key_privs {
            p.retain(|x| cap.contains(x));
        }
        p
    }

    pub fn can(&self, user: &User, path: &str, privilege: &str) -> bool {
        self.privs(user, path).contains(privilege)
    }

    /// Administrator: may change permissions at `/`. Derived, not a flag, so
    /// a custom role granting it is a real admin and the "last admin" guards
    /// still hold.
    pub fn is_admin(&self, user: &User) -> bool {
        self.can(user, "/", "Permissions.Modify")
    }

    /// `is_admin` without the resource and key steps, for use inside `privs`.
    /// Whether the user's privileges at `path` come from an entry below `/`
    /// (on the resource, its kind, or its daemon), rather than from the
    /// grant at the root that every account gets.
    fn granted_below_root(&self, user: &User, path: &str) -> bool {
        let me = format!("user:{}", user.id);
        let groups: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.members.contains(&user.id))
            .map(|g| format!("group:{}", g.name))
            .collect();
        let mine = |e: &AclEntry| e.ugid == me || groups.contains(&e.ugid);
        self.acl.iter().any(|e| {
            mine(e)
                && e.path != "/"
                && (e.path == path || (e.propagate && path.starts_with(&format!("{}/", e.path))))
        })
    }

    fn is_admin_raw(&self, user: &User) -> bool {
        let p = self.acl_privs(user, "/");
        p.contains("Permissions.Modify")
            && user
                .key_privs
                .as_ref()
                .is_none_or(|k| k.contains("Permissions.Modify"))
    }

    pub fn admin_count(&self) -> usize {
        self.users.iter().filter(|u| self.is_admin(u)).count()
    }
}

/// Random id / token helpers (crypto RNG via rand).
pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Readable random name, e.g. "brisk-otter-4f2a". Used when an API key is
/// created without one: a pronounceable label beats a bare hex id in a list,
/// and it needs no uniqueness check because the suffix is random.
pub fn random_name() -> String {
    const ADJECTIVES: [&str; 16] = [
        "amber",
        "brisk",
        "calm",
        "clever",
        "dusty",
        "eager",
        "gentle",
        "hidden",
        "ivory",
        "lucky",
        "mellow",
        "nimble",
        "quiet",
        "rapid",
        "silver",
        "wandering",
    ];
    const NOUNS: [&str; 16] = [
        "otter", "falcon", "cedar", "harbor", "lantern", "meadow", "nebula", "oyster", "pebble",
        "quartz", "raven", "summit", "thicket", "willow", "beacon", "cobalt",
    ];
    let seed = random_hex(3);
    // Two independent nibbles pick the words; the rest becomes the suffix.
    let a = usize::from_str_radix(&seed[0..1], 16).unwrap_or(0);
    let n = usize::from_str_radix(&seed[1..2], 16).unwrap_or(0);
    format!("{}-{}-{}", ADJECTIVES[a], NOUNS[n], &seed[2..])
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn avatar_path(data_dir: &Path, seq: u64, ext: &str) -> PathBuf {
    data_dir.join("avatars").join(format!("avatar-{seq}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> User {
        User {
            id: id.into(),
            username: id.into(),
            password_hash: String::new(),
            nickname: id.into(),
            avatar: None,
            role: String::new(),
            scopes: HashMap::new(),
            quota: Quota::default(),
            key_privs: None,
        }
    }

    fn grant(s: &mut Store, path: &str, who: &str, role: &str) {
        s.acl.push(AclEntry {
            path: path.into(),
            ugid: format!("user:{who}"),
            role: role.into(),
            propagate: true,
        });
    }

    /// An admin `a` and a plain user `u`, both granted at `/`.
    fn store() -> Store {
        let mut s = Store::default();
        s.seed_builtin_roles();
        s.perm_version = PERM_VERSION;
        s.users = vec![user("a"), user("u")];
        grant(&mut s, "/", "a", perm::ADMIN_ROLE);
        grant(&mut s, "/", "u", perm::DEFAULT_ROLE);
        s
    }

    fn own(s: &mut Store, kind: Kind, name: &str, owner: &str) {
        s.resources.push(Resource {
            kind,
            conn: "c".into(),
            pool: String::new(),
            name: name.into(),
            owner: owner.into(),
            suspended: false,
            path: String::new(),
            capacity: 0,
            blank: false,
            created_ms: 0,
        });
    }

    #[test]
    fn a_user_holds_their_own_vm_and_nobody_elses() {
        let mut s = store();
        own(&mut s, Kind::Vm, "mine", "u");
        own(&mut s, Kind::Vm, "theirs", "a");
        let u = user("u");
        assert!(s.can(&u, &perm::vm_path("c", "mine"), "VM.Config"));
        assert!(s.can(&u, &perm::vm_path("c", "mine"), "VM.Console"));
        assert!(!s.can(&u, &perm::vm_path("c", "theirs"), "VM.Audit"));
    }

    /// A user with the default role must not be able to delete, rename,
    /// start or configure someone else's VM or network. The role grants
    /// VM.Allocate and Network.Allocate at / so users can create, and that
    /// propagates onto every resource path, where Allocate means delete.
    #[test]
    fn creating_vms_does_not_let_you_delete_other_peoples() {
        let mut s = store();
        own(&mut s, Kind::Vm, "theirs", "a");
        own(&mut s, Kind::Network, "theirnet", "a");
        let u = user("u");
        for p in ["VM.Allocate", "VM.Config", "VM.PowerMgmt", "VM.Console"] {
            assert!(
                !s.can(&u, &perm::vm_path("c", "theirs"), p),
                "{p} on another's VM"
            );
        }
        for p in ["Network.Allocate", "Network.Config", "Network.Use"] {
            assert!(
                !s.can(&u, &perm::net_path("c", "theirnet"), p),
                "{p} on another's network"
            );
        }
        // Still able to create: the collection paths keep the grant.
        assert!(s.can(&u, "/nodes/c/vms", "VM.Allocate"));
        assert!(s.can(&u, "/nodes/c/networks", "Network.Allocate"));
        // An admin and the owner still can.
        assert!(s.can(&user("a"), &perm::vm_path("c", "theirs"), "VM.Allocate"));
    }

    /// A "NoAccess on this one guest" carve-out must hold whichever way the
    /// guest is addressed. The daemon now resolves a UUID to a guest, so a
    /// request by UUID must be checked against the same ACL key as one by
    /// name -- here, both are the guest's canonical key.
    #[test]
    fn a_carve_out_holds_however_the_guest_is_addressed() {
        let mut s = store();
        grant(&mut s, "/nodes/c", "u", "VMAdmin");
        let secret = "ea15249b-a713-41dc-a808-05619e42eb2a";
        grant(&mut s, &perm::vm_path("c", secret), "u", "NoAccess");
        let u = user("u");
        assert!(
            !s.can(&u, &perm::vm_path("c", secret), "VM.Console"),
            "carved out"
        );
        assert!(
            s.can(&u, &perm::vm_path("c", "other-uuid"), "VM.Console"),
            "the rest of the daemon"
        );
    }

    #[test]
    fn creating_and_reading_are_separate() {
        let s = store();
        let u = user("u");
        // The default role may create but not read other guests.
        assert!(s.can(&u, "/nodes/c/vms", "VM.Allocate"));
        assert!(!s.can(&u, "/nodes/c/vms", "VM.Audit"));
    }

    #[test]
    fn unrecorded_resources_belong_to_the_first_admin() {
        let s = store();
        assert_eq!(s.owner_of(Kind::Vm, "c", "old").as_deref(), Some("a"));
        assert!(!s.can(&user("u"), &perm::vm_path("c", "old"), "VM.Audit"));
    }

    #[test]
    fn sharing_a_vm_is_an_acl_entry_on_its_path() {
        let mut s = store();
        own(&mut s, Kind::Vm, "shared", "a");
        grant(&mut s, &perm::vm_path("c", "shared"), "u", "VMUser");
        let u = user("u");
        let p = perm::vm_path("c", "shared");
        assert!(s.can(&u, &p, "VM.PowerMgmt"));
        assert!(!s.can(&u, &p, "VM.Config"));
    }

    #[test]
    fn suspending_leaves_the_owner_read_only_but_not_the_admin() {
        let mut s = store();
        own(&mut s, Kind::Network, "lan", "u");
        s.resource_mut(Kind::Network, "c", "lan").unwrap().suspended = true;
        let p = perm::net_path("c", "lan");
        assert!(s.can(&user("u"), &p, "Network.Audit"));
        assert!(!s.can(&user("u"), &p, "Network.Use"));
        assert!(!s.can(&user("u"), &p, "Network.Config"));
        assert!(s.can(&user("a"), &p, "Network.Config"));
    }

    #[test]
    fn a_key_never_exceeds_its_owner_and_can_narrow_it() {
        let s = store();
        let mut k = user("u");
        k.key_privs = Some(
            ["Sys.Audit", "Permissions.Modify"]
                .iter()
                .map(|x| x.to_string())
                .collect(),
        );
        assert!(s.can(&k, "/", "Sys.Audit"));
        assert!(!s.can(&k, "/", "Permissions.Modify"), "owner lacks it");
        assert!(!s.can(&k, "/nodes/c/vms", "VM.Allocate"), "key lacks it");
        let mut ka = user("a");
        ka.key_privs = Some(Privs::new());
        assert!(!s.is_admin(&ka), "an empty key is powerless");
    }

    #[test]
    fn admin_is_derived_from_the_privilege_not_a_name() {
        let mut s = store();
        s.roles.push(RoleDef {
            name: "custom".into(),
            builtin: false,
            privs: vec!["Permissions.Modify".into()],
            scopes: HashMap::new(),
        });
        s.users.push(user("c"));
        grant(&mut s, "/", "c", "custom");
        assert!(s.is_admin(&user("c")));
        assert_eq!(s.admin_count(), 2);
    }

    #[test]
    fn an_edited_builtin_role_is_preserved_when_seeding() {
        let mut s = store();
        s.roles
            .iter_mut()
            .find(|r| r.name == perm::ADMIN_ROLE)
            .unwrap()
            .privs
            .clear();
        s.seed_builtin_roles();
        assert!(!s.is_admin(&user("a")));
    }

    #[test]
    fn a_removed_builtin_role_stays_removed_when_seeding() {
        let mut s = store();
        s.roles.retain(|r| r.name != perm::DEFAULT_ROLE);
        s.removed_builtin_roles.push(perm::DEFAULT_ROLE.into());
        s.seed_builtin_roles();
        assert!(!s.roles.iter().any(|r| r.name == perm::DEFAULT_ROLE));
    }

    /// An install from before ACLs: roles with scope maps, a user override,
    /// a scoped key. Everyone must keep the access they had.
    #[test]
    fn an_old_store_migrates_to_equivalent_acls() {
        let json = r#"{
            "users": [
              {"id":"a","username":"root","password_hash":"","role":"admin","nickname":"","avatar":null},
              {"id":"o","username":"op","password_hash":"","role":"operator","nickname":"","avatar":null},
              {"id":"v","username":"viewer","password_hash":"","role":"viewer","nickname":"","avatar":null,
               "scopes":{"storage":"write"}},
              {"id":"c","username":"cus","password_hash":"","role":"netonly","nickname":"","avatar":null}
            ],
            "sessions": {}, "connections": [], "avatar_seq": 0,
            "roles": [
              {"name":"admin","builtin":true,"scopes":{"users":"write","settings":"write","connections":"write","vms":"write","networks":"write","storage":"write"}},
              {"name":"operator","builtin":true,"scopes":{"users":"none","settings":"none","connections":"write","vms":"write","networks":"write","storage":"write"}},
              {"name":"viewer","builtin":true,"scopes":{"users":"none","settings":"none","connections":"read","vms":"read","networks":"read","storage":"read"}},
              {"name":"netonly","scopes":{"networks":"write"}}
            ],
            "api_keys": [{"id":"k","user_id":"a","name":"k","hash":"","prefix":"","scopes":{"users":"none"}}]
        }"#;
        let mut s: Store = serde_json::from_str(json).unwrap();
        s.migrate();
        s.seed_builtin_roles();
        let v = |id: &str| s.users.iter().find(|u| u.id == id).unwrap().clone();
        assert!(s.is_admin(&v("a")));
        let root = s.acl.iter().find(|e| e.ugid == "user:a").unwrap();
        assert_eq!(
            root.role,
            perm::ADMIN_ROLE,
            "a full admin gets the real role"
        );
        assert!(!s.is_admin(&v("o")));
        assert!(
            !s.can(&v("o"), "/", "Permissions.Modify"),
            "migration grants nothing that did not exist before"
        );
        assert!(
            s.can(&v("o"), "/nodes/c/vms/x", "VM.Config"),
            "operator keeps VM write"
        );
        assert!(s.can(&v("v"), "/nodes/c/vms/x", "VM.Audit"));
        assert!(!s.can(&v("v"), "/nodes/c/vms/x", "VM.Config"));
        assert!(
            s.can(&v("v"), "/nodes/c/storage/p", "Datastore.Allocate"),
            "override kept"
        );
        assert!(s.can(&v("c"), "/nodes/c/networks/n", "Network.Config"));
        assert!(!s.can(&v("c"), "/nodes/c/vms/x", "VM.Audit"));
        let k = &s.api_keys[0];
        assert!(!k.privs.contains(&"Permissions.Modify".to_string()));
        assert!(k.privs.contains(&"VM.Config".to_string()));
        // Migration runs once: a second load must not duplicate entries.
        let n = s.acl.len();
        s.migrate();
        assert_eq!(s.acl.len(), n);
    }

    #[test]
    fn loopback_modes_local_user_is_an_administrator() {
        let s = Store::default();
        let local = crate::auth::local_user();
        assert!(s.is_admin(&local));
        assert!(s.can(&local, &perm::vm_path("c", "x"), "VM.Config"));
    }

    #[test]
    fn a_store_written_before_activity_existed_still_loads() {
        let json = r#"{"users":[],"sessions":{},"connections":[],"api_keys":[],"avatar_seq":0}"#;
        let parsed: Store = serde_json::from_str(json).expect("old store parses");
        assert!(parsed.activity.is_empty());
    }

    /// A container ownership row from an older version cannot deserialize
    /// (`Kind` has no `Ct`), and must not stop the whole store loading.
    #[test]
    fn a_leftover_container_ownership_row_is_dropped_on_load() {
        let dir = std::env::temp_dir().join(format!(
            "store-test-{}-{}",
            std::process::id(),
            random_hex(4)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("store.json"),
            r#"{"users":[],"sessions":{},"connections":[],"api_keys":[],"avatar_seq":0,
                "resources":[
                  {"kind":"ct","conn":"c","name":"ct1","owner":"u"},
                  {"kind":"vm","conn":"c","name":"vm1","owner":"u"}
                ]}"#,
        )
        .unwrap();
        let s = Store::load(&dir).expect("store with a stale container row loads");
        assert_eq!(s.resources.len(), 1);
        assert_eq!(s.resources[0].name, "vm1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
