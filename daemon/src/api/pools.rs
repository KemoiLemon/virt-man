//! `/pools`: storage pools as the daemon's own directories.
//!
//! The wire shape is the one the SPA has always seen — libvirt's field names,
//! `title`, `persistent`, `capacity`/`allocation`/`available` — but nothing
//! here talks to an external storage driver. The store in `crate::pools` is the
//! source of truth, and a read always scans the directory, so a file that
//! arrived by `cp` or a sync is visible immediately.

use axum::Json;
use axum::Router;
use axum::extract::{Extension, Path, Query};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;

use super::ApiError;
use crate::disks;
use crate::pools::{Pool, Pools};
use crate::qemu::Manager;

pub fn routes() -> Router {
    Router::new()
        .route("/pools", get(list).post(create))
        .route("/pools/{name}", get(get_one).delete(delete))
        .route("/pools/{name}/xml", get(get_xml).put(put_xml))
        .route("/pools/{name}/start", post(start))
        .route("/pools/{name}/stop", post(stop))
        .route("/pools/{name}/refresh", post(refresh))
        .route("/pools/{name}/title", axum::routing::put(set_title))
}

fn pool_json(key: &str, p: &Pool, pools: &Pools) -> Value {
    // `root()` is the authority: a root replaced by a symlink is not active.
    let active = p.active && pools.root(key).is_ok();
    let (capacity, allocation, available) = pools.stats(key).unwrap_or((None, None, None));
    json!({
        "name": key,
        "uuid": key,
        "title": if p.title.is_empty() { None } else { Some(p.title.as_str()) },
        "active": active,
        // The store is the definition; a pool without one cannot exist.
        "persistent": true,
        "capacity": capacity,
        "allocation": allocation,
        "available": available,
    })
}

async fn list(Extension(pools): Extension<Pools>) -> Result<Json<Value>, ApiError> {
    let arr: Vec<Value> = pools
        .list()
        .iter()
        .map(|(key, p)| pool_json(key, p, &pools))
        .collect();
    Ok(Json(json!(arr)))
}

async fn get_one(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    let p = pools.get(&key).expect("resolved pool is present");
    Ok(Json(pool_json(&key, &p, &pools)))
}

/// The libvirt-shaped definition, rendered from the store.
fn pool_xml(key: &str, path: &std::path::Path) -> String {
    format!(
        "<pool type='dir'>\n  <name>{key}</name>\n  <uuid>{key}</uuid>\n  \
         <target><path>{}</path></target>\n</pool>",
        crate::domainxml::esc(&path.to_string_lossy())
    )
}

async fn get_xml(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    let p = pools.get(&key).expect("resolved pool is present");
    Ok(Json(json!({"name": key, "xml": pool_xml(&key, &p.path)})))
}

/// The first element's text, unescaped.
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let i = xml.find(&open)?;
    let rest = &xml[i + open.len()..];
    let end = rest.find(&format!("</{tag}>"))?;
    Some(crate::domainxml::unescape(rest[..end].trim()))
}

/// What a pool definition must say: a canonical UUID name and an absolute
/// directory. A non-directory type is refused here, once, with a reason —
/// LVM and filesystem pool types are not implemented natively.
fn parse_pool_xml(xml: &str) -> Result<(String, String), String> {
    let open_end = xml.find('>').map(|e| e + 1).unwrap_or(xml.len());
    let kind = disks::attr(&xml[..open_end], "type").unwrap_or("dir");
    if kind != "dir" {
        return Err(format!(
            "pool type '{kind}' is not implemented natively; only directory pools are supported"
        ));
    }
    let name = tag_text(xml, "name")
        .or_else(|| tag_text(xml, "uuid"))
        .ok_or("the pool definition has no <name>")?;
    // The name is the key the web server, grants and URLs use, so it must be
    // the canonical UUID shape, not whatever a caller supplied.
    let parsed = uuid::Uuid::parse_str(&name)
        .map_err(|_| format!("pool name '{name}' is not a UUID; every pool is named by one"))?;
    if parsed.hyphenated().to_string() != name {
        return Err(format!("pool name '{name}' is not a canonical UUID"));
    }
    let path = tag_text(xml, "path").ok_or("the pool definition has no <target><path>")?;
    Ok((name, path))
}

/// Define and start a pool. `?title=` sets its display name, which a pool
/// definition cannot carry.
async fn create(
    Extension(pools): Extension<Pools>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let (name, raw_path) = parse_pool_xml(&body).map_err(ApiError::bad_request)?;
    // Duplicate first: a retried create must say "already exists", not complain
    // that the path overlaps the pool it is duplicating.
    if pools.lock().pools.contains_key(&name) {
        return Err(ApiError::conflict(format!(
            "a pool '{name}' already exists"
        )));
    }
    let title = q.get("title").cloned().unwrap_or_default();
    let title = title.trim().to_string();
    let path = pools
        .prepare_root(&raw_path, None)
        .map_err(ApiError::bad_request)?;
    {
        let mut s = pools.lock();
        if s.pools.contains_key(&name) {
            return Err(ApiError::conflict(format!(
                "a pool '{name}' already exists"
            )));
        }
        s.pools.insert(
            name.clone(),
            Pool {
                title,
                path,
                autostart: true,
                active: true,
            },
        );
        pools.save(&s).map_err(ApiError::internal)?;
    }
    Ok(Json(json!({"name": name, "uuid": name, "started": true})))
}

async fn put_xml(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    let (named, raw_path) = parse_pool_xml(&body).map_err(ApiError::bad_request)?;
    if named != key {
        return Err(ApiError::bad_request(format!(
            "the definition names pool '{named}', not '{key}'"
        )));
    }
    let path = pools
        .prepare_root(&raw_path, Some(&key))
        .map_err(ApiError::bad_request)?;
    {
        let mut s = pools.lock();
        let Some(p) = s.pools.get_mut(&key) else {
            return Err(ApiError::not_found(format!("no pool '{name}'")));
        };
        p.path = path;
        pools.save(&s).map_err(ApiError::internal)?;
    }
    Ok(Json(json!({"name": key, "defined": true})))
}

async fn start(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    let p = pools.get(&key).expect("resolved pool is present");
    std::fs::create_dir_all(&p.path)
        .map_err(|e| ApiError::internal(format!("{}: {e}", p.path.display())))?;
    // Creating the directory is the explicit "start" that may recreate a
    // missing root; a root that is now a symlink or resolves elsewhere is
    // still refused.
    pools.root(&key).map_err(ApiError::bad_request)?;
    {
        let mut s = pools.lock();
        if let Some(p) = s.pools.get_mut(&key) {
            p.active = true;
        }
        pools.save(&s).map_err(ApiError::internal)?;
    }
    Ok(Json(json!({"name": key, "started": true})))
}

/// Mark it inactive. The directory and every file in it stay: a pool's stop
/// is a bookkeeping change, never a removal.
async fn stop(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    {
        let mut s = pools.lock();
        if let Some(p) = s.pools.get_mut(&key) {
            p.active = false;
        }
        pools.save(&s).map_err(ApiError::internal)?;
    }
    Ok(Json(json!({"name": key, "stopped": true})))
}

/// A read always rescans, so there is nothing cached to refresh; the call
/// stays for the SPA, which offers the action.
async fn refresh(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    Ok(Json(json!({"name": key, "refreshed": true})))
}

#[derive(serde::Deserialize)]
struct TitleReq {
    title: String,
}

/// Rename a pool as people see it. Its key (its UUID) stays put, so no guest's
/// path, no grant and no URL changes.
async fn set_title(
    Extension(pools): Extension<Pools>,
    Path(name): Path<String>,
    Json(req): Json<TitleReq>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    {
        let mut s = pools.lock();
        if let Some(p) = s.pools.get_mut(&key) {
            p.title = req.title.trim().to_string();
        }
        pools.save(&s).map_err(ApiError::internal)?;
    }
    Ok(Json(json!({"uuid": key, "title": req.title.trim()})))
}

/// The first volume of the pool a guest still holds, and those guests.
///
/// Split out so the decision can be tested with a seeded holder map, without a
/// manager: deletion of a pool must refuse exactly when some guest names one of
/// its files (by path or by pool+volume).
fn pool_in_use(
    volumes: &[crate::pools::Volume],
    held: &HashMap<String, Vec<String>>,
    key: &str,
) -> Option<(String, Vec<String>)> {
    volumes.iter().find_map(|v| {
        let users = disks::holders_of(held, key, &v.name, &v.path.to_string_lossy());
        (!users.is_empty()).then(|| (v.name.clone(), users))
    })
}

/// Delete the pool's definition, never its files.
///
/// Refused while a guest still holds a volume: the files stay behind by
/// design, but a pool that still backs a guest cannot silently disappear from
/// the tree, or the next volume list would have nowhere to look.
async fn delete(
    Extension(pools): Extension<Pools>,
    Extension(manager): Extension<Manager>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key = pools
        .resolve_key(&name)
        .ok_or_else(|| ApiError::not_found(format!("no pool '{name}'")))?;
    let volumes = pools.scan(&key);
    if !volumes.is_empty() {
        let held = disks::holders_by_label(&disks::guest_definitions(&manager), &pools);
        if let Some((volume, users)) = pool_in_use(&volumes, &held, &key) {
            return Err(ApiError::conflict(format!(
                "'{volume}' is in use by {} — detach it first",
                users.join(", ")
            )));
        }
    }
    {
        let mut s = pools.lock();
        s.pools.remove(&key);
        pools.save(&s).map_err(ApiError::internal)?;
    }
    // The pool is gone; a stale title record is clutter, not a reason to fail
    // a delete that already succeeded.
    if let Err(e) = crate::volume_titles::clear_pool(&key) {
        tracing::warn!("pool {key}: volume titles not cleared: {e}");
    }
    Ok(Json(json!({"name": name, "undefined": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";

    #[test]
    fn a_dir_pool_round_trips_through_xml() {
        let xml = pool_xml(U, std::path::Path::new("/srv/images"));
        let (name, path) = parse_pool_xml(&xml).unwrap();
        assert_eq!(name, U);
        assert_eq!(path, "/srv/images");
    }

    #[test]
    fn a_non_directory_pool_says_it_is_not_implemented() {
        let xml = format!(
            "<pool type='logical'><name>{U}</name><target><path>/dev/vg</path></target></pool>"
        );
        let err = parse_pool_xml(&xml).unwrap_err();
        assert!(err.contains("not implemented natively"), "{err}");
        assert!(err.contains("logical"), "{err}");
    }

    #[test]
    fn the_pool_name_must_be_a_canonical_uuid() {
        let template = |n: &str| {
            format!("<pool type='dir'><name>{n}</name><target><path>/srv/x</path></target></pool>")
        };
        assert!(parse_pool_xml(&template("default")).is_err());
        assert!(parse_pool_xml(&template(&U.to_ascii_uppercase())).is_err());
        // The `<uuid>` element alone still names it.
        let xml =
            format!("<pool type='dir'><uuid>{U}</uuid><target><path>/srv/x</path></target></pool>");
        assert_eq!(parse_pool_xml(&xml).unwrap().0, U);
    }

    #[test]
    fn a_pool_is_kept_when_a_guest_holds_one_of_its_volumes() {
        let key = U;
        let volume = crate::pools::Volume {
            name: "disk.raw".into(),
            path: std::path::PathBuf::from("/srv/images/disk.raw"),
            capacity: 1,
            allocation: 1,
        };
        let mut held = HashMap::new();
        held.insert(
            "file:/srv/images/disk.raw".to_string(),
            vec!["web".to_string()],
        );
        assert_eq!(
            pool_in_use(std::slice::from_ref(&volume), &held, key),
            Some(("disk.raw".into(), vec!["web".into()])),
            "the guest names the file by path"
        );
        held.clear();
        held.insert(format!("volume:{key}/disk.raw"), vec!["db".to_string()]);
        assert_eq!(
            pool_in_use(&[volume], &held, key),
            Some(("disk.raw".into(), vec!["db".into()])),
            "or by pool + volume"
        );
        assert_eq!(
            pool_in_use(&[], &held, key),
            None,
            "an empty pool is never in use"
        );
    }

    /// Through the real router: create, read and delete, with the store as the
    /// only state. No manager and no root.
    #[tokio::test]
    async fn the_routes_create_read_and_delete_a_pool() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-pools")
            .join(format!("routes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("images");
        let pools = Pools::load(dir.join("state/pools.json")).unwrap();
        // Only the delete path reads guest definitions; no VM is defined here.
        let manager = crate::qemu::Manager::with_qemu(
            dir.join("qemu"),
            dir.join("qemu-state"),
            "/nonexistent/qemu",
        )
        .unwrap();
        let app = || {
            super::routes()
                .layer(Extension(pools.clone()))
                .layer(Extension(manager.clone()))
        };

        let xml = format!(
            "<pool type='dir'><name>{U}</name><uuid>{U}</uuid>\
             <target><path>{}</path></target></pool>",
            root.display()
        );
        let get = |req: Request<Body>| {
            let app = app();
            async move {
                let r = app.oneshot(req).await.unwrap();
                let status = r.status();
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
                )
            }
        };

        let (status, created) = get(Request::post("/pools?title=Fast")
            .body(Body::from(xml))
            .unwrap())
        .await;
        assert_eq!(status, 200, "{created}");
        assert!(root.is_dir(), "the root directory was created");

        // A retried create reports the duplicate, not an overlap.
        let (status, again) = get(Request::post("/pools?title=Fast")
            .body(Body::from(format!(
                "<pool type='dir'><name>{U}</name>\
                     <target><path>{}</path></target></pool>",
                root.display()
            )))
            .unwrap())
        .await;
        assert_eq!(status, 409, "{again}");
        assert!(
            again["error"].as_str().unwrap().contains("already exists"),
            "{again}"
        );

        let (status, list) = get(Request::get("/pools").body(Body::empty()).unwrap()).await;
        assert_eq!(status, 200, "{list}");
        assert_eq!(list[0]["name"], U, "{list}");
        assert_eq!(list[0]["title"], "Fast");
        assert_eq!(list[0]["active"], true);
        assert_eq!(list[0]["persistent"], true);

        let (status, v) = get(Request::get(format!("/pools/{U}/xml"))
            .body(Body::empty())
            .unwrap())
        .await;
        assert_eq!(status, 200, "{v}");
        let path = root.to_string_lossy().to_string();
        assert!(v["xml"].as_str().unwrap().contains(&path), "{v}");

        // Delete drops the definition, not the directory.
        let (status, gone) = get(Request::delete(format!("/pools/{U}"))
            .body(Body::empty())
            .unwrap())
        .await;
        assert_eq!(status, 200, "{gone}");
        assert!(root.is_dir(), "deleting a pool never deletes its files");
        assert!(pools.get(U).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
