use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type Scopes = HashMap<String, Permission>;

/// Every permission scope the backend checks. This is the single source of
/// truth: the roles API hands it to the UI so the editor can never invent a
/// scope name that nothing enforces.
pub const SCOPES: [&str; 6] = [
    "users",
    "settings",
    "connections",
    "vms",
    "networks",
    "storage",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Read,
    Write,
    None,
}

impl Permission {
    /// Ordering for intersecting two permissions. None < Read < Write.
    fn rank(self) -> u8 {
        match self {
            Permission::None => 0,
            Permission::Read => 1,
            Permission::Write => 2,
        }
    }

    /// The weaker of two permissions, used to cap an API key by its owner.
    pub fn min(self, other: Permission) -> Permission {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }
}

/// A role is data, not an enum: admins can add their own and retune the
/// built-in three. `builtin` roles may be edited but never renamed or
/// deleted, so a store always has something to fall back on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleDef {
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub scopes: Scopes,
}

fn scopes_of(pairs: &[(&str, Permission)]) -> Scopes {
    pairs
        .iter()
        .map(|(s, p)| ((*s).to_string(), *p))
        .collect()
}

/// The three built-in roles, matching the permissions this app shipped with
/// before roles were stored as data.
pub fn builtin_roles() -> Vec<RoleDef> {
    use Permission::{None as No, Read, Write};
    vec![
        RoleDef {
            name: "admin".into(),
            builtin: true,
            scopes: scopes_of(&SCOPES.map(|s| (s, Write))),
        },
        RoleDef {
            name: "operator".into(),
            builtin: true,
            scopes: scopes_of(&SCOPES.map(|s| {
                (s, if matches!(s, "users" | "settings") { No } else { Write })
            })),
        },
        RoleDef {
            name: "viewer".into(),
            builtin: true,
            scopes: scopes_of(&SCOPES.map(|s| {
                (s, if matches!(s, "users" | "settings") { No } else { Read })
            })),
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    /// Role name, resolved against `Store::roles`. A plain string so custom
    /// roles work and so existing stores ("admin"/"operator"/"viewer") load
    /// unchanged.
    pub role: String,
    pub nickname: String,
    pub avatar: Option<String>,
    #[serde(default)]
    pub scopes: Scopes,
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
    pub user_id: String,
    pub name: String,
    /// sha256 hex of the key; the key itself is shown only once.
    pub hash: String,
    pub prefix: String,
    /// Per-scope ceiling for this key. Empty means "whatever the owner has".
    /// A key can only ever narrow its owner's access, never widen it.
    #[serde(default)]
    pub scopes: Scopes,
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
}

impl Store {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("store.json");
        let mut store: Store = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?
        } else {
            Store::default()
        };
        store.seed_builtin_roles();
        Ok(store)
    }

    /// Add any missing built-in role. Runs on every load so stores written
    /// before roles existed gain them, and so a deleted builtin comes back.
    pub fn seed_builtin_roles(&mut self) {
        for def in builtin_roles() {
            if !self.roles.iter().any(|r| r.name == def.name) {
                self.roles.push(def);
            }
        }
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

    /// Per-user override first, then the user's role. Anything the role does
    /// not mention is denied: fail closed, so adding a scope to SCOPES without
    /// granting it cannot silently hand out access.
    pub fn permission(&self, user: &User, scope: &str) -> Permission {
        if let Some(p) = user.scopes.get(scope) {
            return *p;
        }
        self.role(&user.role)
            .and_then(|r| r.scopes.get(scope).copied())
            .unwrap_or(Permission::None)
    }

    /// Permission upgrade ordering for the "can write" check.
    pub fn can(&self, user: &User, scope: &str, write: bool) -> bool {
        if write {
            self.permission(user, scope) == Permission::Write
        } else {
            self.permission(user, scope) != Permission::None
        }
    }

    /// Who counts as an administrator: whoever can write the users scope.
    /// Derived rather than name-matched, so a custom role granting users:write
    /// is a real admin and the "last admin" guards still hold.
    pub fn is_admin(&self, user: &User) -> bool {
        self.permission(user, "users") == Permission::Write
    }

    pub fn admin_count(&self) -> usize {
        self.users.iter().filter(|u| self.is_admin(u)).count()
    }

    /// Effective scopes for a request authenticated by an API key: the key's
    /// ceiling intersected with the owner's own access, materialised as
    /// per-user overrides so `permission()` needs no extra plumbing.
    pub fn key_effective_scopes(&self, owner: &User, key: &ApiKey) -> Scopes {
        SCOPES
            .iter()
            .map(|scope| {
                let owner_perm = self.permission(owner, scope);
                let capped = match key.scopes.get(*scope) {
                    Some(k) => k.min(owner_perm),
                    // Unlisted scope inherits the owner unchanged.
                    None => owner_perm,
                };
                ((*scope).to_string(), capped)
            })
            .collect()
    }
}

/// Random id / token helpers (crypto RNG via rand).
pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
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

    fn user(role: &str) -> User {
        User {
            id: "u1".into(),
            username: "u".into(),
            password_hash: String::new(),
            role: role.into(),
            nickname: "u".into(),
            avatar: None,
            scopes: Scopes::new(),
        }
    }

    fn store_with(users: Vec<User>) -> Store {
        let mut s = Store::default();
        s.seed_builtin_roles();
        s.users = users;
        s
    }

    fn key(scopes: &[(&str, Permission)]) -> ApiKey {
        ApiKey {
            id: "k1".into(),
            user_id: "u1".into(),
            name: "k".into(),
            hash: String::new(),
            prefix: "vm_".into(),
            scopes: scopes_of(scopes),
        }
    }

    #[test]
    fn builtin_roles_keep_their_historical_permissions() {
        let s = store_with(vec![]);
        let admin = user("admin");
        let operator = user("operator");
        let viewer = user("viewer");

        for scope in SCOPES {
            assert_eq!(s.permission(&admin, scope), Permission::Write, "admin {scope}");
        }
        assert_eq!(s.permission(&operator, "users"), Permission::None);
        assert_eq!(s.permission(&operator, "settings"), Permission::None);
        assert_eq!(s.permission(&operator, "vms"), Permission::Write);
        assert_eq!(s.permission(&viewer, "users"), Permission::None);
        assert_eq!(s.permission(&viewer, "vms"), Permission::Read);
    }

    #[test]
    fn per_user_override_beats_the_role() {
        let mut s = store_with(vec![]);
        let mut u = user("viewer");
        u.scopes.insert("vms".into(), Permission::Write);
        assert_eq!(s.permission(&u, "vms"), Permission::Write);
        // ...and can also restrict an admin.
        let mut a = user("admin");
        a.scopes.insert("users".into(), Permission::None);
        assert_eq!(s.permission(&a, "users"), Permission::None);
        assert!(!s.is_admin(&a));
        s.users = vec![a];
        assert_eq!(s.admin_count(), 0);
    }

    #[test]
    fn an_unknown_role_denies_everything() {
        // Fail closed: a dangling role name must not grant access.
        let s = store_with(vec![]);
        let u = user("no-such-role");
        for scope in SCOPES {
            assert_eq!(s.permission(&u, scope), Permission::None, "{scope}");
        }
    }

    #[test]
    fn a_scope_the_role_omits_is_denied() {
        let mut s = store_with(vec![]);
        s.roles.push(RoleDef {
            name: "partial".into(),
            builtin: false,
            scopes: scopes_of(&[("vms", Permission::Read)]),
        });
        let u = user("partial");
        assert_eq!(s.permission(&u, "vms"), Permission::Read);
        assert_eq!(s.permission(&u, "storage"), Permission::None);
    }

    #[test]
    fn a_key_can_narrow_its_owner() {
        let s = store_with(vec![]);
        let owner = user("admin");
        let k = key(&[("vms", Permission::Read)]);
        let eff = s.key_effective_scopes(&owner, &k);
        assert_eq!(eff["vms"], Permission::Read, "narrowed");
        // Unlisted scopes inherit the owner untouched.
        assert_eq!(eff["storage"], Permission::Write);
    }

    #[test]
    fn a_key_can_never_exceed_its_owner() {
        let s = store_with(vec![]);
        let owner = user("viewer"); // vms: read
        let k = key(&[("vms", Permission::Write), ("users", Permission::Write)]);
        let eff = s.key_effective_scopes(&owner, &k);
        assert_eq!(eff["vms"], Permission::Read, "capped down to the owner");
        assert_eq!(eff["users"], Permission::None, "owner has none, so none");
    }

    #[test]
    fn demoting_the_owner_weakens_existing_keys() {
        let s = store_with(vec![]);
        let k = key(&[("vms", Permission::Write)]);
        let before = s.key_effective_scopes(&user("operator"), &k);
        assert_eq!(before["vms"], Permission::Write);
        let after = s.key_effective_scopes(&user("viewer"), &k);
        assert_eq!(after["vms"], Permission::Read, "follows the owner down");
    }

    #[test]
    fn permission_min_is_an_ordering() {
        use Permission::{None as No, Read, Write};
        assert_eq!(Write.min(Read), Read);
        assert_eq!(Read.min(Write), Read);
        assert_eq!(Read.min(No), No);
        assert_eq!(Write.min(Write), Write);
    }

    #[test]
    fn admin_is_derived_from_users_write_not_the_role_name() {
        let mut s = store_with(vec![]);
        s.roles.push(RoleDef {
            name: "custom-admin".into(),
            builtin: false,
            scopes: scopes_of(&[("users", Permission::Write)]),
        });
        let u = user("custom-admin");
        assert!(s.is_admin(&u), "a custom role granting users:write is an admin");
        s.users = vec![u];
        assert_eq!(s.admin_count(), 1);
    }

    #[test]
    fn seeding_restores_a_missing_builtin() {
        let mut s = Store::default();
        s.seed_builtin_roles();
        s.roles.retain(|r| r.name != "admin");
        assert!(s.role("admin").is_none());
        s.seed_builtin_roles();
        assert!(s.role("admin").is_some(), "builtin comes back on load");
    }
}
