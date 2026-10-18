//! Read-only discovery of ordinary host directories, separate from libvirt pools.
use axum::{Json, Router, extract::Path, routing::get};
use serde::Serialize;
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
};

use super::ApiError;

#[derive(Debug, Serialize)]
pub struct HostPool {
    pub name: String,
    pub path: PathBuf,
    pub capacity: u64,
    pub available: u64,
    pub volumes: Vec<HostVolume>,
}

#[derive(Debug, Serialize)]
pub struct HostVolume {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Explicit roots are supplied by VIRT_MAN_HOST_POOL_ROOTS as `name=/path,...`.
fn roots() -> Result<Vec<(String, PathBuf)>, String> {
    let roots: Vec<_> = std::env::var("VIRT_MAN_HOST_POOL_ROOTS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|entry| {
            let (name, path) = entry.split_once('=')?;
            (!name.is_empty() && !path.is_empty()).then(|| (name.to_owned(), PathBuf::from(path)))
        })
        .collect();
    let mut names = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for (name, path) in &roots {
        if !names.insert(name) {
            return Err(format!("duplicate host pool name: {name}"));
        }
        let canonical =
            fs::canonicalize(path).map_err(|e| format!("canonicalize pool root: {e}"))?;
        if paths
            .iter()
            .any(|p: &PathBuf| p.starts_with(&canonical) || canonical.starts_with(p))
        {
            return Err("overlapping host pool roots".into());
        }
        paths.push(canonical);
    }
    Ok(roots)
}

pub fn routes() -> Router {
    Router::new()
        .route("/host/pools", get(list))
        .route("/host/pools/{name}", get(read))
}

async fn list() -> Result<Json<Vec<HostPool>>, ApiError> {
    roots()
        .map_err(ApiError::bad_request)?
        .into_iter()
        .map(|(name, root)| discover(&name, &root).map_err(ApiError::internal))
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

async fn read(Path(name): Path<String>) -> Result<Json<HostPool>, ApiError> {
    let (name, root) = roots()
        .map_err(ApiError::bad_request)?
        .into_iter()
        .find(|(n, _)| n == &name)
        .ok_or_else(|| ApiError::not_found("host pool not found"))?;
    discover(&name, &root).map(Json).map_err(ApiError::internal)
}

fn discover(name: &str, root: &FsPath) -> Result<HostPool, String> {
    let root = fs::canonicalize(root).map_err(|e| format!("canonicalize pool root: {e}"))?;
    if !root.is_dir() {
        return Err("pool root is not a directory".into());
    }
    let mut volumes = Vec::new();
    for entry in fs::read_dir(&root).map_err(|e| format!("read pool root: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let canonical = fs::canonicalize(&path).map_err(|e| e.to_string())?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "volume escapes pool root: {}",
                entry.file_name().to_string_lossy()
            ));
        }
        let meta = fs::metadata(&canonical).map_err(|e| e.to_string())?;
        if meta.is_file() {
            volumes.push(HostVolume {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: canonical,
                size: meta.len(),
            });
        }
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    let (capacity, available) = fs_space(&root).ok_or_else(|| {
        format!(
            "statvfs pool root failed: {}",
            std::io::Error::last_os_error()
        )
    })?;
    Ok(HostPool {
        name: name.to_owned(),
        path: root,
        capacity,
        available,
        volumes,
    })
}

// std has no portable statvfs wrapper; report root metadata's filesystem space via libc.
fn fs_space(path: &FsPath) -> Option<(u64, u64)> {
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
    #[test]
    fn discovers_external_file_and_rejects_escape() {
        let temp = std::env::temp_dir().join(format!("host-pool-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let root = temp.join("pool");
        let outside = temp.join("outside");
        fs::create_dir(&root).unwrap();
        fs::write(&root.join("external.qcow2"), b"image").unwrap();
        fs::write(&outside, b"outside").unwrap();
        let result = discover("test", &root).unwrap();
        assert_eq!(result.volumes[0].name, "external.qcow2");
        assert_eq!(result.volumes[0].size, 5);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        #[cfg(unix)]
        assert!(discover("test", &root).unwrap_err().contains("escapes"));
        let _ = fs::remove_dir_all(temp);
    }
}
