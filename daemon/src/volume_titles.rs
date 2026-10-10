//! Display names for storage volumes, keyed by pool UUID and volume name.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const TITLES_PATH: &str = "/etc/virt-man/volume-titles.json";
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
}
