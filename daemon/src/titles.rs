//! Display names for storage pools.
//!
//! A pool's libvirt name is its UUID (so two can share a display name), but
//! unlike a guest or a network a pool definition has no `<title>` to keep the
//! display name in -- libvirt accepts one and silently drops it (measured,
//! libvirt 12.7). So the daemon keeps them itself, keyed by UUID, in
//! `/etc/virt-man-daemon/titles.json`, and reports them in the pool list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const TITLES_PATH: &str = "/etc/virt-man-daemon/titles.json";

fn store() -> &'static Mutex<(PathBuf, HashMap<String, String>)> {
    static S: OnceLock<Mutex<(PathBuf, HashMap<String, String>)>> = OnceLock::new();
    S.get_or_init(|| {
        let path = PathBuf::from(TITLES_PATH);
        Mutex::new((path.clone(), load(&path)))
    })
}

fn load(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Atomic like the other stores: temp file then rename.
fn save(path: &Path, map: &HashMap<String, String>) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(map).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

pub fn get(uuid: &str) -> Option<String> {
    store().lock().expect("titles lock").1.get(uuid).cloned()
}

/// Set (or, with an empty title, clear) a pool's display name.
pub fn set(uuid: &str, title: &str) -> Result<(), String> {
    let mut s = store().lock().expect("titles lock");
    let t = title.trim();
    if t.is_empty() {
        s.1.remove(uuid);
    } else {
        s.1.insert(uuid.to_string(), t.to_string());
    }
    let (path, map) = &*s;
    save(path, map)
}

/// Drop titles for pools that no longer exist.
pub fn retain(live: &[String]) -> Result<(), String> {
    let mut s = store().lock().expect("titles lock");
    let before = s.1.len();
    s.1.retain(|k, _| live.contains(k));
    if s.1.len() == before {
        return Ok(());
    }
    let (path, map) = &*s;
    save(path, map)
}

#[cfg(test)]
mod tests {
    #[test]
    fn titles_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("vm-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("titles.json");
        let mut m = std::collections::HashMap::new();
        m.insert("u1".to_string(), "Fast SSD".to_string());
        super::save(&path, &m).unwrap();
        assert_eq!(
            super::load(&path).get("u1").map(String::as_str),
            Some("Fast SSD")
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file renamed away"
        );
        assert!(
            super::load(&dir.join("missing.json")).is_empty(),
            "missing file is empty"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
