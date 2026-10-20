//! Storage pools, owned by this daemon.
//!
//! A pool is a directory on the host plus a little metadata: its display name,
//! whether it autostarts, and whether it is active. There is no storage driver
//! involved — a cached volume list and space figures stayed stale until
//! something refreshed them, while a directory scan is both cheaper and always
//! true, so a file that arrived by `cp`, aria2 or a sync is visible at once.
//!
//! The store at `<data_dir>/pools.json` is the source of truth. Keys are the
//! UUIDs the web UI addresses resources by; the file is written atomically and
//! mode 0600.
//!
//! Two safety rules shape the file handling:
//!
//! - **A pool root is a real directory the operator chose.** It is
//!   canonicalised and created on explicit create/start, refused when it
//!   overlaps another pool or sits in a reserved place (`/proc`, `/root`, the
//!   daemon's own data dir, ...). It is re-checked on *every* use: if the
//!   stored canonical path is now a symlink or resolves elsewhere, operations
//!   refuse with "re-point the pool" instead of following it.
//! - **A scan never follows a symlink.** A file is listed only when it is a
//!   regular file at that path; a symlink is skipped, so a link planted in a
//!   pool cannot make the daemon read, or later delete, something outside it.
//!   Dotfiles are the atomic-write temporaries and are never volumes.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Where a qcow2 header keeps the guest-visible size, big-endian.
const QCOW2_MAGIC: &[u8; 4] = b"QFI\xfb";
const QCOW2_VSIZE_OFFSET: usize = 24;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub pools: BTreeMap<String, Pool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pool {
    /// Display name. A pool has no definition of its own to keep a `<title>`
    /// in, so it lives here.
    #[serde(default)]
    pub title: String,
    /// Canonical host directory. Every volume is a file directly inside it.
    pub path: PathBuf,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub path: PathBuf,
    /// Guest-visible size: the qcow2 virtual size when the header says so,
    /// the file's own length otherwise.
    pub capacity: u64,
    /// Disk blocks actually used (`st_blocks * 512`), so a sparse image does
    /// not count as full.
    pub allocation: u64,
}

#[derive(Clone)]
pub struct Pools {
    path: PathBuf,
    store: Arc<Mutex<Store>>,
}

impl Pools {
    /// Read the store. A missing file is an empty store; a malformed one is an
    /// error rather than a silent reset, which would forget every pool.
    pub fn load(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let store = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        Ok(Self {
            path,
            store: Arc::new(Mutex::new(store)),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("pools lock")
    }

    /// Atomic, like the other stores: temp file then rename, so a crash cannot
    /// leave half a JSON document that the next start refuses to parse.
    pub fn save(&self, store: &Store) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        // A temp file left by a crash must not keep its old permissions.
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        use std::io::Write;
        f.write_all(&text)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("rename {}: {e}", self.path.display()))
    }

    /// Every pool, in key order, so list output is stable.
    pub fn list(&self) -> Vec<(String, Pool)> {
        self.lock()
            .pools
            .iter()
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect()
    }

    /// The key a request names: the UUID itself, or a pool's display name.
    /// An ambiguous title — two pools an operator gave the same display name —
    /// resolves to nothing, so a write cannot land on the wrong one.
    pub fn resolve_key(&self, key: &str) -> Option<String> {
        let s = self.lock();
        if s.pools.contains_key(key) {
            return Some(key.to_string());
        }
        let mut matches = s
            .pools
            .iter()
            .filter(|(_, p)| !p.title.is_empty() && p.title == key)
            .map(|(k, _)| k);
        let first = matches.next()?;
        matches.next().is_none().then(|| first.clone())
    }

    pub fn get(&self, key: &str) -> Option<Pool> {
        let k = self.resolve_key(key)?;
        self.lock().pools.get(&k).cloned()
    }

    /// A pool's root, revalidated on every operation.
    ///
    /// The stored path was canonical when the pool was created. If it is now a
    /// symlink, or it resolves somewhere else (a root renamed and replaced), the
    /// pool is refused rather than followed: otherwise a link planted in the
    /// pool's place would redirect every scan, upload and delete to whatever it
    /// points at. The admin re-points the pool (PUT xml) to accept the change.
    pub fn root(&self, pool: &str) -> Result<PathBuf, String> {
        let p = self.get(pool).ok_or_else(|| format!("no pool '{pool}'"))?;
        checked_root(&p.path)
    }

    /// Startup only marks autostart pools active; it never creates a missing
    /// root. A missing root usually means the filesystem it lived on is not
    /// mounted, and creating a fresh empty directory there would hide that and
    /// present an empty pool. An explicit start (or create) may create it.
    pub fn startup(&self) {
        let mut s = self.lock();
        let mut changed = false;
        for (key, p) in s.pools.iter_mut() {
            if !p.autostart {
                continue;
            }
            let usable = checked_root(&p.path).is_ok();
            if usable != p.active {
                p.active = usable;
                changed = true;
            }
            if !usable {
                tracing::warn!(
                    pool = %key,
                    path = %p.path.display(),
                    "pool root is missing or changed; left inactive (start the pool to recreate it)"
                );
            }
        }
        if changed && let Err(e) = self.save(&s) {
            tracing::warn!("save pools: {e}");
        }
    }

    /// Validate a requested root and return its canonical form: absolute,
    /// created if missing, a real directory, not reserved, not overlapping
    /// another pool.
    pub fn prepare_root(&self, path: &str, except: Option<&str>) -> Result<PathBuf, String> {
        let raw = Path::new(path.trim());
        if raw.as_os_str().is_empty() {
            return Err("give the pool a directory".into());
        }
        if !raw.is_absolute() {
            return Err(format!("'{}' is not an absolute path", raw.display()));
        }
        if raw.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("'{}' must not contain '..'", raw.display()));
        }
        // Check where the path actually resolves *before* creating anything: a
        // symlinked ancestor (say /srv/link -> /) could otherwise make
        // create_dir_all write into a reserved directory, and the refusal
        // would come after the damage.
        let resolved = resolve_existing(raw);
        self.check_reserved(&resolved)?;
        self.check_overlap(&resolved, except)?;
        if let Ok(m) = std::fs::symlink_metadata(raw)
            && m.file_type().is_file()
        {
            return Err(format!("'{}' is a file, not a directory", raw.display()));
        }
        std::fs::create_dir_all(raw).map_err(|e| format!("create {}: {e}", raw.display()))?;
        let canonical =
            std::fs::canonicalize(raw).map_err(|e| format!("resolve {}: {e}", raw.display()))?;
        if !canonical.is_dir() {
            return Err(format!("'{}' is not a directory", canonical.display()));
        }
        // A symlinked root resolves somewhere; refuse if that place is
        // reserved or inside another pool.
        self.check_reserved(&canonical)?;
        self.check_overlap(&canonical, except)?;
        Ok(canonical)
    }

    fn deny_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = [
            "/proc", "/sys", "/dev", "/run", "/etc", "/root", "/boot", "/tmp", "/var/tmp",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        // The daemon's own state and configuration: a pool there would let a
        // volume overwrite the store, the token or a certificate.
        roots.push(PathBuf::from("/etc/virt-man-daemon"));
        roots.push(PathBuf::from("/etc/virt-man"));
        // The web server's data dir holds users, connections, API keys and
        // pinned daemon certificates: `/var/lib/virt-man` (DESIGN.md "State and
        // storage"), with `/var/lib/virt-man-web` refused too in case a
        // deployment uses that spelling. Refused by convention, not by reading
        // the other service's config. `/home` stays allowed: pool roots are
        // arbitrary directories an administrator chose, including one under a
        // home directory on a single-user host.
        roots.push(PathBuf::from("/var/lib/virt-man"));
        roots.push(PathBuf::from("/var/lib/virt-man-web"));
        if let Some(data) = self.path.parent() {
            roots.push(data.to_path_buf());
        }
        roots
    }

    fn check_reserved(&self, p: &Path) -> Result<(), String> {
        if p == Path::new("/") {
            return Err("the filesystem root cannot be a pool".into());
        }
        for r in self.deny_roots() {
            if p == r || p.starts_with(&r) {
                return Err(format!(
                    "{} is reserved for the host or the daemon",
                    p.display()
                ));
            }
        }
        Ok(())
    }

    fn check_overlap(&self, p: &Path, except: Option<&str>) -> Result<(), String> {
        let s = self.lock();
        for (key, other) in &s.pools {
            if except == Some(key.as_str()) {
                continue;
            }
            if p.starts_with(&other.path) || other.path.starts_with(p) {
                let shown = if other.title.is_empty() {
                    key.as_str()
                } else {
                    other.title.as_str()
                };
                return Err(format!(
                    "{} overlaps pool '{shown}' at {}",
                    p.display(),
                    other.path.display()
                ));
            }
        }
        Ok(())
    }

    /// Files directly in the pool, regular ones only, sorted by name.
    ///
    /// Dotfiles are skipped: they are the daemon's in-progress upload/fetch
    /// temporaries (`.name.uuid.tmp`), which are not volumes and must never
    /// appear, be picked as a disk or be deleted through the API. Names that
    /// `safe_name` rejects are skipped too, so everything listed is addressable.
    pub fn scan(&self, pool: &str) -> Vec<Volume> {
        let Ok(root) = self.root(pool) else {
            return vec![];
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return vec![];
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`, never `metadata`: a symlink is skipped
            // rather than followed out of the pool.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') || safe_name(&name).is_err() {
                continue;
            }
            let allocation = meta.blocks().saturating_mul(512);
            let capacity = qcow2_capacity(&path).unwrap_or(meta.len());
            out.push(Volume {
                name,
                path,
                capacity,
                allocation,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Capacity, allocation and available bytes. All `None` when the root is
    /// gone, so a page can say "inactive" rather than "0 bytes".
    ///
    /// Deliberately not `scan()`: this is polled every second for every pool in
    /// the sidebar, and an allocation total needs only each file's block count.
    /// Opening every qcow2 header (and sorting the list) buys nothing here.
    pub fn stats(&self, key: &str) -> Option<(Option<u64>, Option<u64>, Option<u64>)> {
        let root = self.root(key).ok()?;
        let allocation = allocation_of(&root);
        let (capacity, available) = match fs_space(&root) {
            Some((c, a)) => (Some(c), Some(a)),
            None => (None, None),
        };
        Some((capacity, Some(allocation), available))
    }

    /// One regular file in the pool, if it exists. A symlink, a directory, a
    /// dotfile (an in-progress temporary) or a missing name is `None`, so a
    /// handler answers 404 rather than touching something the pool does not
    /// own.
    pub fn volume(&self, pool: &str, name: &str) -> Option<Volume> {
        self.checked_volume(pool, name).ok()
    }

    /// [`Pools::volume`] with the reason a root is unusable kept, for the
    /// write paths whose caller can show it.
    pub fn checked_volume(&self, pool: &str, name: &str) -> Result<Volume, String> {
        let root = self.root(pool)?;
        let name = safe_name(name)?;
        if name.starts_with('.') {
            return Err(format!("'{name}' is a temporary file, not a volume"));
        }
        let path = root.join(name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|_| format!("no volume '{name}' in {pool}"))?;
        if !meta.file_type().is_file() {
            return Err(format!("'{name}' is not a regular file"));
        }
        Ok(Volume {
            name: name.to_string(),
            capacity: qcow2_capacity(&path).unwrap_or(meta.len()),
            allocation: meta.blocks().saturating_mul(512),
            path,
        })
    }

    /// The file behind a pool + volume reference, as an absolute path.
    pub fn volume_path(&self, pool: &str, name: &str) -> Option<PathBuf> {
        self.volume(pool, name).map(|v| v.path)
    }

    /// Whether anything at all exists under that name, including a symlink or
    /// a directory — creates and fetches refuse to touch any of them.
    pub fn entry_exists(&self, pool: &str, name: &str) -> bool {
        let Ok(root) = self.root(pool) else {
            return false;
        };
        let Ok(name) = safe_name(name) else {
            return false;
        };
        std::fs::symlink_metadata(root.join(name)).is_ok()
    }

    /// The canonical path of a regular file inside one of our pools. Used by
    /// the delete paths: a domain's disk is removed only when a pool owns it,
    /// so a definition pointing at `/etc/shadow` (or any other host path) can
    /// never make a guest delete reach it.
    pub fn owned_file(&self, path: &str) -> Option<PathBuf> {
        let real = std::fs::canonicalize(path).ok()?;
        if !std::fs::metadata(&real).ok()?.is_file() {
            return None;
        }
        let roots: Vec<PathBuf> = self.lock().pools.values().map(|p| p.path.clone()).collect();
        roots
            .iter()
            .any(|r| checked_root(r).is_ok_and(|root| real.starts_with(root)))
            .then_some(real)
    }

    /// Create an empty raw file of `size` bytes (sparse: `set_len`, no write).
    pub fn create_raw(&self, pool: &str, name: &str, size: u64) -> Result<PathBuf, String> {
        let p = self.get(pool).ok_or_else(|| format!("no pool '{pool}'"))?;
        if !p.active {
            return Err(format!("pool '{pool}' is not active"));
        }
        let root = self.root(pool)?;
        let name = safe_name(name)?;
        let path = root.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!("a volume named '{name}' already exists"));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        if let Err(e) = file.set_len(size) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("size {}: {e}", path.display()));
        }
        Ok(path)
    }
}

/// A pool root as it must be at every use: a real directory (not a symlink),
/// still resolving exactly to the canonical path the pool was created with.
fn checked_root(stored: &Path) -> Result<PathBuf, String> {
    let meta = std::fs::symlink_metadata(stored).map_err(|_| {
        format!(
            "pool root {} is missing; start the pool to recreate it",
            stored.display()
        )
    })?;
    if !meta.file_type().is_dir() {
        return Err(root_changed(stored));
    }
    let real = std::fs::canonicalize(stored).map_err(|_| root_changed(stored))?;
    if real != stored {
        return Err(root_changed(stored));
    }
    Ok(real)
}

fn root_changed(stored: &Path) -> String {
    format!(
        "pool root {} changed or is a symlink; re-point the pool",
        stored.display()
    )
}

/// Disk blocks used by the regular files directly in `root`. No qcow2 header
/// reads: this feeds the polled allocation figure, not a volume list.
fn allocation_of(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| std::fs::symlink_metadata(e.path()).ok())
        .filter(|m| m.file_type().is_file())
        .map(|m| m.blocks().saturating_mul(512))
        .sum()
}

/// A file name, not a path: one component, no separators, no traversal.
pub fn safe_name(name: &str) -> Result<&str, String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        return Err(format!("'{name}' is not a usable volume name"));
    }
    Ok(name)
}

/// The guest-visible size in a qcow2 header, when the file has one.
///
/// A qcow2 file's own length is its allocation, not the disk size a guest
/// sees: a 40 GiB disk with 2 GiB written is a ~2 GiB file. The header carries
/// the real size as a big-endian u64 at offset 24. Anything that is not a
/// qcow2 (or not a complete header) falls back to the file length.
pub fn qcow2_capacity(path: &Path) -> Option<u64> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 32];
    f.read_exact(&mut header).ok()?;
    if &header[0..4] != QCOW2_MAGIC {
        return None;
    }
    let v = u64::from_be_bytes(header[QCOW2_VSIZE_OFFSET..32].try_into().ok()?);
    (v > 0).then_some(v)
}

/// Canonicalise the part of `path` that exists, keeping the missing tail.
///
/// Used before `create_dir_all` so a symlinked ancestor is resolved first: the
/// reserved-place check must see where the path really leads, not its spelling.
fn resolve_existing(path: &Path) -> PathBuf {
    let mut probe = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(&probe) {
            for part in tail.iter().rev() {
                real.push(part);
            }
            return real;
        }
        let Some(name) = probe.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        if !probe.pop() {
            return path.to_path_buf();
        }
        tail.push(name);
    }
}

/// Total and available bytes on the filesystem holding `path`.
///
/// std has no portable statvfs wrapper; libc's is the honest source, and it
/// reports the same figures `df` does.
fn fs_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let unit = s.f_frsize as u64;
    Some((
        (s.f_blocks as u64).saturating_mul(unit),
        (s.f_bavail as u64).saturating_mul(unit),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch directory per test: these run in parallel.
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        // Not /tmp: it is on the reserved list, and these tests create real
        // pool roots. `target/` is build output and already ignored.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-pools");
        let _ = std::fs::create_dir_all(&base);
        let dir = base.join(format!("{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(dir: &Path) -> Pools {
        Pools::load(dir.join("state/pools.json")).unwrap()
    }

    fn add(pools: &Pools, key: &str, title: &str, path: PathBuf) {
        let mut s = pools.lock();
        s.pools.insert(
            key.into(),
            Pool {
                title: title.into(),
                path,
                autostart: true,
                active: true,
            },
        );
        pools.save(&s).unwrap();
    }

    const U: &str = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";

    #[test]
    fn a_root_is_created_canonicalised_and_kept_out_of_reserved_places() {
        let dir = scratch("validate");
        let pools = open(&dir);
        let root = dir.join("images");
        let got = pools.prepare_root(root.to_str().unwrap(), None).unwrap();
        assert!(got.is_dir(), "missing directories are created");
        assert_eq!(got, std::fs::canonicalize(&root).unwrap());

        for reserved in [
            "/proc/virtman-pool",
            "/root/virtman-pool",
            "/boot/x",
            "/tmp/x",
        ] {
            assert!(
                pools.prepare_root(reserved, None).is_err(),
                "{reserved} is reserved"
            );
        }
        assert!(
            pools
                .prepare_root(dir.join("state").to_str().unwrap(), None)
                .is_err(),
            "the daemon's own data directory is reserved"
        );
        assert!(
            pools.prepare_root("relative/path", None).is_err(),
            "only absolute paths"
        );
        assert!(
            pools
                .prepare_root(dir.join("a/../b").to_str().unwrap(), None)
                .is_err(),
            "traversal is refused outright"
        );
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(pools.prepare_root(file.to_str().unwrap(), None).is_err());
    }

    #[test]
    fn a_missing_tail_is_resolved_under_its_symlinked_ancestor() {
        let dir = scratch("resolve");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        assert_eq!(
            resolve_existing(&dir.join("link/a/b")),
            std::fs::canonicalize(&real).unwrap().join("a/b"),
            "the existing prefix is canonicalised, the missing tail kept"
        );
        assert_eq!(
            resolve_existing(&real),
            std::fs::canonicalize(&real).unwrap()
        );
    }

    #[test]
    fn overlapping_roots_are_refused() {
        let dir = scratch("overlap");
        let pools = open(&dir);
        let a = pools
            .prepare_root(dir.join("a").to_str().unwrap(), None)
            .unwrap();
        add(&pools, U, "A", a.clone());

        let inside = dir.join("a/child");
        assert!(
            pools.prepare_root(inside.to_str().unwrap(), None).is_err(),
            "a root inside another pool is refused"
        );
        let outside = dir.join("a/../b");
        assert!(
            pools.prepare_root(outside.to_str().unwrap(), None).is_err(),
            "a root containing another pool is refused"
        );
        assert!(
            pools.prepare_root(a.to_str().unwrap(), Some(U)).is_ok(),
            "a pool may keep its own root"
        );
    }

    #[test]
    fn the_store_round_trips_with_private_permissions() {
        let dir = scratch("save");
        let pools = open(&dir);
        let root = pools
            .prepare_root(dir.join("images").to_str().unwrap(), None)
            .unwrap();
        add(&pools, U, "Fast SSD", root.clone());

        let path = dir.join("state/pools.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the store is private");
        assert!(!path.with_extension("json.tmp").exists(), "renamed away");

        let again = Pools::load(path).unwrap();
        let p = again.get(U).unwrap();
        assert_eq!(p.title, "Fast SSD");
        assert_eq!(p.path, root);
        assert!(p.autostart && p.active);
    }

    #[test]
    fn a_duplicate_title_is_ambiguous_rather_than_the_first_match() {
        let dir = scratch("dup-title");
        let pools = open(&dir);
        let a = pools
            .prepare_root(dir.join("a").to_str().unwrap(), None)
            .unwrap();
        let b = pools
            .prepare_root(dir.join("b").to_str().unwrap(), None)
            .unwrap();
        add(&pools, "key-a", "Shared", a);
        add(&pools, "key-b", "Shared", b);
        assert_eq!(pools.resolve_key("key-a").as_deref(), Some("key-a"));
        assert_eq!(
            pools.resolve_key("Shared"),
            None,
            "two pools with one display name cannot be addressed by it"
        );
        assert!(pools.get("Shared").is_none());
    }

    #[test]
    fn a_pool_is_found_by_its_key_or_its_title() {
        let dir = scratch("title");
        let pools = open(&dir);
        let root = pools
            .prepare_root(dir.join("images").to_str().unwrap(), None)
            .unwrap();
        add(&pools, U, "Fast SSD", root);

        assert_eq!(pools.resolve_key(U).as_deref(), Some(U));
        assert_eq!(pools.resolve_key("Fast SSD").as_deref(), Some(U));
        assert_eq!(pools.resolve_key("nothing"), None);
        assert_eq!(pools.lock().pools[U].title, "Fast SSD");
    }

    #[test]
    fn startup_leaves_a_missing_root_inactive_and_never_creates_it() {
        let dir = scratch("autostart");
        let pools = open(&dir);
        let root = dir.join("images");
        std::fs::create_dir_all(&root).unwrap();
        add(&pools, U, "A", root.clone());
        std::fs::remove_dir_all(&root).unwrap();
        pools.startup();
        assert!(
            !root.exists(),
            "an unmounted filesystem must not look like an empty pool"
        );
        let p = pools.get(U).unwrap();
        assert!(!p.active, "left inactive rather than silently recreated");
        assert!(p.autostart, "autostart is still wanted for the next boot");

        // A root replaced by a file is refused just the same.
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(&root, b"not a directory").unwrap();
        pools.startup();
        assert!(!pools.get(U).unwrap().active);
    }

    #[test]
    fn a_root_replaced_by_a_symlink_is_refused_everywhere() {
        let dir = scratch("hijack");
        let pools = open(&dir);
        let root = dir.join("images");
        let inside = pools.prepare_root(root.to_str().unwrap(), None).unwrap();
        add(&pools, U, "A", inside);
        std::fs::write(root.join("disk.raw"), b"real").unwrap();

        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("victim.raw"), b"outside").unwrap();
        std::fs::rename(&root, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();

        let err = pools.root(U).unwrap_err();
        assert!(err.contains("re-point the pool"), "{err}");
        assert!(pools.scan(U).is_empty(), "not listed through the link");
        assert!(pools.volume(U, "victim.raw").is_none());
        assert!(!pools.entry_exists(U, "victim.raw"));
        assert!(
            pools.create_raw(U, "new.raw", 1).is_err(),
            "a new volume is not created through the link"
        );
        assert!(
            pools.checked_volume(U, "victim.raw").is_err(),
            "not deletable through the link"
        );
        assert_eq!(
            std::fs::read(elsewhere.join("victim.raw")).unwrap(),
            b"outside",
            "nothing outside the pool was touched"
        );
        assert!(!elsewhere.join("new.raw").exists());
        assert!(
            !pools.lock().pools[U].active || pools.root(U).is_err(),
            "the store still names the original root only"
        );
    }

    #[test]
    fn a_scan_lists_regular_files_and_skips_links_and_directories() {
        let dir = scratch("scan");
        let pools = open(&dir);
        let root = dir.join("images");
        std::fs::create_dir_all(&root).unwrap();
        add(&pools, U, "A", root.clone());
        add(&pools, "other-key", "B", dir.join("elsewhere"));

        std::fs::write(root.join("disk.raw"), vec![7u8; 10]).unwrap();
        // A synthetic qcow2 header: magic, then the virtual size at 24.
        let mut qcow = [0u8; 64];
        qcow[0..4].copy_from_slice(b"QFI\xfb");
        qcow[24..32].copy_from_slice(&(40u64 << 30).to_be_bytes());
        std::fs::write(root.join("disk.qcow2"), qcow).unwrap();
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        let outside = dir.join("outside.bin");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape.raw")).unwrap();

        // In-progress temporaries and unaddressable names are not volumes.
        std::fs::write(root.join(".disk.raw.abc.tmp"), b"partial").unwrap();
        std::fs::write(root.join("bad\u{7}name.raw"), b"x").unwrap();

        let vols = pools.scan(U);
        let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["disk.qcow2", "disk.raw"], "{names:?}");
        assert!(
            pools.volume(U, ".disk.raw.abc.tmp").is_none(),
            "a temporary is not addressable"
        );

        let qcow2 = &vols[0];
        assert_eq!(qcow2.capacity, 40 << 30, "the header's virtual size wins");
        assert_eq!(qcow2.path, root.join("disk.qcow2"));
        let raw = &vols[1];
        assert_eq!(raw.capacity, 10, "a non-qcow2 file reports its length");
        assert!(
            raw.allocation > 0,
            "a written file consumes blocks: {}",
            raw.allocation
        );
    }

    #[test]
    fn stats_sums_blocks_without_reading_headers() {
        let dir = scratch("stats");
        let pools = open(&dir);
        let root = dir.join("images");
        add(
            &pools,
            U,
            "A",
            pools.prepare_root(root.to_str().unwrap(), None).unwrap(),
        );
        std::fs::write(root.join("disk.raw"), vec![7u8; 4096]).unwrap();
        // A header that is deliberately nonsense for a capacity read: stats
        // must only need the block count, and a broken header falls back to the
        // file length for the volume list anyway.
        std::fs::write(root.join("disk.qcow2"), [0u8; 64]).unwrap();

        let (capacity, allocation, available) = pools.stats(U).unwrap();
        assert!(capacity.unwrap() > 0);
        assert!(available.unwrap() > 0);
        let sum: u64 = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| std::fs::symlink_metadata(e.path()).unwrap().blocks() * 512)
            .sum();
        assert_eq!(allocation.unwrap(), sum, "every regular file's blocks");
    }

    #[test]
    fn a_volume_resolves_only_to_a_regular_file_in_its_pool() {
        let dir = scratch("volume");
        let pools = open(&dir);
        let root = dir.join("images");
        add(
            &pools,
            U,
            "A",
            pools.prepare_root(root.to_str().unwrap(), None).unwrap(),
        );
        std::fs::write(root.join("ok.raw"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("images/ok.raw"), root.join("link.raw")).unwrap();

        assert!(pools.volume(U, "ok.raw").is_some());
        assert!(
            pools.volume(U, "link.raw").is_none(),
            "a symlink is not a volume"
        );
        assert!(pools.volume(U, "../outside").is_none(), "no traversal");
        assert!(pools.entry_exists(U, "link.raw"), "but it still exists");
    }

    #[test]
    fn owned_file_draws_the_pool_boundary() {
        let dir = scratch("owned");
        let pools = open(&dir);
        let root = dir.join("images");
        add(
            &pools,
            U,
            "A",
            pools.prepare_root(root.to_str().unwrap(), None).unwrap(),
        );
        std::fs::write(root.join("disk.qcow2"), b"x").unwrap();
        let outside = dir.join("host-file");
        std::fs::write(&outside, b"x").unwrap();

        assert_eq!(
            pools.owned_file(root.join("disk.qcow2").to_str().unwrap()),
            Some(std::fs::canonicalize(root.join("disk.qcow2")).unwrap())
        );
        assert!(
            pools.owned_file(outside.to_str().unwrap()).is_none(),
            "a file outside every pool is not ours to delete"
        );
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert!(
            pools
                .owned_file(root.join("escape").to_str().unwrap())
                .is_none(),
            "a link is resolved, and its target is outside"
        );
    }

    #[test]
    fn raw_volumes_are_sparse_and_never_overwrite() {
        let dir = scratch("create");
        let pools = open(&dir);
        let root = dir.join("images");
        add(
            &pools,
            U,
            "A",
            pools.prepare_root(root.to_str().unwrap(), None).unwrap(),
        );

        let path = pools.create_raw(U, "disk.raw", 1 << 30).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1 << 30);
        assert!(
            std::fs::metadata(&path).unwrap().blocks() < 1 << 10,
            "sparse: set_len allocates no data blocks"
        );
        assert!(
            pools.create_raw(U, "disk.raw", 1 << 30).is_err(),
            "an existing file is never overwritten"
        );
        assert!(
            pools.create_raw(U, "a/b", 1).is_err(),
            "a name is one component"
        );
    }

    #[test]
    fn an_inactive_pool_refuses_writes() {
        let dir = scratch("inactive");
        let pools = open(&dir);
        let root = pools
            .prepare_root(dir.join("images").to_str().unwrap(), None)
            .unwrap();
        add(&pools, U, "A", root);
        pools.lock().pools.get_mut(U).unwrap().active = false;
        assert!(pools.create_raw(U, "x.raw", 1).is_err());
    }

    #[test]
    fn a_synthetic_qcow2_header_reports_the_virtual_size() {
        let dir = scratch("qcow2");
        let path = dir.join("disk.qcow2");
        let mut qcow = [0u8; 32];
        qcow[0..4].copy_from_slice(b"QFI\xfb");
        qcow[24..32].copy_from_slice(&(8u64 << 30).to_be_bytes());
        std::fs::write(&path, qcow).unwrap();
        assert_eq!(qcow2_capacity(&path), Some(8 << 30));

        std::fs::write(dir.join("raw.raw"), [0u8; 32]).unwrap();
        assert_eq!(qcow2_capacity(&dir.join("raw.raw")), None, "bad magic");
        std::fs::write(dir.join("short.bin"), b"QFI").unwrap();
        assert_eq!(qcow2_capacity(&dir.join("short.bin")), None, "short file");
    }
}
