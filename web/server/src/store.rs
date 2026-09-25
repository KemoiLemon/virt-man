use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type Scopes = HashMap<String, Permission>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Read,
    Write,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Role {
    Admin,
    Operator,
    #[default]
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
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
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub users: Vec<User>,
    /// session token (sha256 hex) -> user id. Opaque token stored hashed.
    pub sessions: HashMap<String, String>,
    pub connections: Vec<Connection>,
    pub api_keys: Vec<ApiKey>,
    pub avatar_seq: u64,
}

impl Store {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("store.json");
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            serde_json::from_str(&text)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))
        } else {
            Ok(Store::default())
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

    /// Role + per-user scope override. Admin always gets write unless a scope
    /// override explicitly says otherwise (so admins can be restricted too).
    pub fn permission(&self, user: &User, scope: &str) -> Permission {
        if let Some(p) = user.scopes.get(scope) {
            return *p;
        }
        match user.role {
            Role::Admin => Permission::Write,
            Role::Operator => match scope {
                "users" | "settings" => Permission::None,
                _ => Permission::Write,
            },
            Role::Viewer => match scope {
                "users" | "settings" => Permission::None,
                _ => Permission::Read,
            },
        }
    }

    /// Permission upgrade ordering for the "can write" check.
    pub fn can(&self, user: &User, scope: &str, write: bool) -> bool {
        if write {
            self.permission(user, scope) == Permission::Write
        } else {
            self.permission(user, scope) != Permission::None
        }
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
