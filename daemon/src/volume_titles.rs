//! Display names for storage volumes, keyed by pool UUID and volume name.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const TITLES_PATH: &str = "/etc/virt-man-daemon/volume-titles.json";
type Titles = HashMap<String, String>;

fn key(pool: &str, volume: &str) -> String {
    format!("{pool}/{volume}")
}
fn store() -> &'static Mutex<(PathBuf, Titles)> {
    static S: OnceLock<Mutex<(PathBuf, Titles)>> = OnceLock::new();
    S.get_or_init(|| {
        let path = PathBuf::from(TITLES_PATH);
        let titles = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new((path, titles))
    })
}
fn save(path: &Path, titles: &Titles) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(titles).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}
pub fn get(pool: &str, volume: &str) -> Option<String> {
    store()
        .lock()
        .expect("volume titles lock")
        .1
        .get(&key(pool, volume))
        .cloned()
}
pub fn set(pool: &str, volume: &str, title: &str) -> Result<(), String> {
    let mut s = store().lock().expect("volume titles lock");
    let k = key(pool, volume);
    if title.trim().is_empty() {
        s.1.remove(&k);
    } else {
        s.1.insert(k, title.trim().to_string());
    }
    let (path, titles) = &*s;
    save(path, titles)
}

/// Forget every title of a deleted pool: the keys name volumes that no longer
/// belong to anything, and the file would otherwise keep them forever.
pub fn clear_pool(pool: &str) -> Result<(), String> {
    let mut s = store().lock().expect("volume titles lock");
    let prefix = format!("{pool}/");
    let before = s.1.len();
    s.1.retain(|k, _| !k.starts_with(&prefix));
    if s.1.len() == before {
        return Ok(());
    }
    let (path, titles) = &*s;
    save(path, titles)
}

/// Point the store at a test file, returning the previous one so the test can
/// put it back. Only tests call this; production always uses `TITLES_PATH`.
#[cfg(test)]
fn replace_store(path: PathBuf, titles: Titles) -> (PathBuf, Titles) {
    let mut s = store().lock().expect("volume titles lock");
    std::mem::replace(&mut *s, (path, titles))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_titles_are_scoped_to_pool_and_clearable() {
        let dir = std::env::temp_dir().join(format!("vm-volume-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("titles.json");
        let mut titles = Titles::new();
        titles.insert(key("p1", "disk"), "Root disk".into());
        save(&path, &titles).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Root disk")
        );
        assert_ne!(key("p1", "disk"), key("p2", "disk"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The real `clear_pool`, against a temp store: it forgets exactly the
    /// deleted pool's volumes and leaves every other key alone.
    #[test]
    fn clear_pool_forgets_only_the_deleted_pools_volumes() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-pools")
            .join(format!("volume-titles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("titles.json");
        let mut titles = Titles::new();
        titles.insert(key("p1", "disk"), "Root disk".into());
        titles.insert(key("p1", "iso"), "Media".into());
        titles.insert(key("p2", "disk"), "Other pool".into());
        titles.insert(key("p10", "disk"), "Not p1".into());
        save(&path, &titles).unwrap();

        let previous = replace_store(path.clone(), titles.clone());
        let outcome = clear_pool("p1");
        let left: Titles = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        replace_store(previous.0, previous.1);

        outcome.unwrap();
        assert!(!left.contains_key(&key("p1", "disk")));
        assert!(!left.contains_key(&key("p1", "iso")));
        assert_eq!(
            left.get(&key("p2", "disk")).map(String::as_str),
            Some("Other pool"),
            "another pool's title is kept"
        );
        assert_eq!(
            left.get(&key("p10", "disk")).map(String::as_str),
            Some("Not p1"),
            "a prefix match is keyed by pool, not by string prefix"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
