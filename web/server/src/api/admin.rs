//! The administrative area: groups, permissions (ACL entries), every
//! user-owned resource across every daemon, and the addresses guests hold.
//! All of it needs `Permissions.Modify` at `/`.

use axum::extract::{Path, Query, State};
use axum::middleware;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::guard::{daemon_get, enc};
use crate::perm::{AclEntry, Group, Kind, Resource};
use crate::state::AppState;
use crate::store::Connection;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/groups", get(groups).post(create_group))
        .route("/groups/{name}", put(update_group).delete(delete_group))
        .route("/acl", get(acl).put(add_acl).delete(remove_acl))
        .route("/resources", get(resources).put(set_resource))
        .route("/ips", get(ips))
        .route("/usage", get(usage))
        .layer(middleware::from_fn(auth::require_priv(
            state.clone(),
            "/",
            "Permissions.Modify",
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

// ---------------------------------------------------------------- groups

async fn groups(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.read().groups))
}

#[derive(Deserialize)]
struct GroupReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    members: Vec<String>,
}

fn valid_group(store: &crate::store::Store, req: &GroupReq) -> Result<String, ApiError> {
    let name = req.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request(
            "group name may only contain letters, digits, - and _",
        ));
    }
    if let Some(m) = req.members.iter().find(|m| store.user_by_id(m).is_none()) {
        return Err(ApiError::bad_request(format!("no such user: {m}")));
    }
    Ok(name.to_string())
}

async fn create_group(
    State(state): State<AppState>,
    Json(req): Json<GroupReq>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let name = valid_group(&store, &req)?;
    if store.groups.iter().any(|g| g.name == name) {
        return Err(ApiError::conflict("group already exists"));
    }
    let g = Group {
        name,
        comment: req.comment.trim().into(),
        members: req.members,
    };
    store.groups.push(g.clone());
    store.save(&state.data_dir)?;
    Ok(Json(json!(g)))
}

/// Replace a group's comment and members (and rename it, carrying its
/// grants). Refused if it would leave no administrator.
async fn update_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<GroupReq>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(idx) = store.groups.iter().position(|g| g.name == name) else {
        return Err(ApiError::not_found("no such group"));
    };
    let next = if req.name.trim().is_empty() {
        name.clone()
    } else {
        valid_group(&store, &req)?
    };
    if let Some(m) = req.members.iter().find(|m| store.user_by_id(m).is_none()) {
        return Err(ApiError::bad_request(format!("no such user: {m}")));
    }
    if next != name && store.groups.iter().any(|g| g.name == next) {
        return Err(ApiError::conflict("group already exists"));
    }
    let before = (store.groups.clone(), store.acl.clone());
    let mut members = req.members;
    members.sort();
    members.dedup();
    store.groups[idx] = Group {
        name: next.clone(),
        comment: req.comment.trim().into(),
        members,
    };
    let (from, to) = (format!("group:{name}"), format!("group:{next}"));
    for e in store.acl.iter_mut().filter(|e| e.ugid == from) {
        e.ugid = to.clone();
    }
    if store.admin_count() == 0 {
        (store.groups, store.acl) = before;
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }
    let g = store.groups[idx].clone();
    store.save(&state.data_dir)?;
    Ok(Json(json!(g)))
}

async fn delete_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(idx) = store.groups.iter().position(|g| g.name == name) else {
        return Err(ApiError::not_found("no such group"));
    };
    let before = (store.groups.clone(), store.acl.clone());
    store.groups.remove(idx);
    let ugid = format!("group:{name}");
    store.acl.retain(|e| e.ugid != ugid);
    if store.admin_count() == 0 {
        (store.groups, store.acl) = before;
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": name})))
}

// ---------------------------------------------------------------- acl

async fn acl(State(state): State<AppState>) -> Json<Value> {
    let store = state.read();
    let entries: Vec<Value> = store
        .acl
        .iter()
        .map(|e| {
            let label = match e.ugid.split_once(':') {
                Some(("user", id)) => store
                    .user_by_id(id)
                    .map(|u| u.username.clone())
                    .unwrap_or_else(|| id.to_string()),
                Some((_, g)) => format!("@{g}"),
                None => e.ugid.clone(),
            };
            json!({
                "path": e.path, "ugid": e.ugid, "role": e.role,
                "propagate": e.propagate, "label": label,
            })
        })
        .collect();
    Json(json!(entries))
}

/// ACL paths are `/`, `/nodes`, or `/nodes/<conn>[/<kind>[/<name>]]`.
fn valid_path(p: &str) -> bool {
    if p == "/" || p == "/nodes" {
        return true;
    }
    let seg: Vec<&str> = p.trim_start_matches('/').split('/').collect();
    match seg.as_slice() {
        ["nodes", c] => !c.is_empty(),
        ["nodes", c, k] => {
            !c.is_empty() && matches!(*k, "vms" | "networks" | "storage" | "bridges")
        }
        ["nodes", c, k, n] => {
            !c.is_empty()
                && !n.is_empty()
                && matches!(*k, "vms" | "networks" | "storage" | "bridges")
        }
        _ => false,
    }
}

#[derive(Deserialize)]
struct AclReq {
    path: String,
    ugid: String,
    #[serde(default)]
    role: String,
    #[serde(default = "yes")]
    propagate: bool,
}

fn yes() -> bool {
    true
}

/// Add (or replace) the grant of a role to one user or group at one path.
async fn add_acl(
    State(state): State<AppState>,
    Json(req): Json<AclReq>,
) -> Result<Json<Value>, ApiError> {
    let path = req.path.trim().trim_end_matches('/').to_string();
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    if !valid_path(&path) {
        return Err(ApiError::bad_request(format!(
            "not a permission path: {path}"
        )));
    }
    let mut store = state.write();
    let known = match req.ugid.split_once(':') {
        Some(("user", id)) => store.user_by_id(id).is_some(),
        Some(("group", g)) => store.groups.iter().any(|x| x.name == g),
        _ => false,
    };
    if !known {
        return Err(ApiError::bad_request("no such user or group"));
    }
    if store.role(&req.role).is_none() {
        return Err(ApiError::bad_request(format!("no such role: {}", req.role)));
    }
    let before = store.acl.clone();
    store
        .acl
        .retain(|e| !(e.path == path && e.ugid == req.ugid && e.role == req.role));
    store.acl.push(AclEntry {
        path,
        ugid: req.ugid,
        role: req.role,
        propagate: req.propagate,
    });
    if store.admin_count() == 0 {
        store.acl = before;
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }
    store.save(&state.data_dir)?;
    Ok(Json(json!({"ok": true})))
}

async fn remove_acl(
    State(state): State<AppState>,
    Json(req): Json<AclReq>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let before = store.acl.clone();
    store
        .acl
        .retain(|e| !(e.path == req.path && e.ugid == req.ugid && e.role == req.role));
    if store.acl.len() == before.len() {
        return Err(ApiError::not_found("no such permission"));
    }
    if store.admin_count() == 0 {
        store.acl = before;
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }
    store.save(&state.data_dir)?;
    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------- resources

/// Records younger than this are never pruned: a daemon list fetched just
/// before a create can arrive after it.
const PRUNE_GRACE_MS: u64 = 60_000;

/// Whether a connection's daemon is known to be up. Unknown (no probe yet)
/// counts as up, so a fresh start does not hide a healthy server; a daemon
/// already marked offline is skipped rather than called. That skip is what
/// keeps the admin panel fast while a daemon is down: the health probe has
/// already decided, and re-asking the daemon just waits for the timeout again.
fn online(state: &AppState, id: &str) -> bool {
    state
        .health
        .read()
        .expect("health lock poisoned")
        .get(id)
        .is_none_or(|h| h.online)
}

/// Every guest, network, pool and volume on every reachable daemon, with its
/// owner. Records for things a daemon no longer has are dropped here, so
/// ownership does not leak onto a later resource of the same name.
async fn resources(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conns: Vec<Connection> = state.read().connections.clone();
    let mut out = vec![];
    let mut seen: Vec<(Kind, String, String, String)> = vec![];
    let mut answered: Vec<String> = vec![];
    for c in &conns {
        if !online(&state, &c.id) {
            continue; // known down: do not wait for it again
        }
        let (Ok(doms), Ok(nets), Ok(pools)) = (
            daemon_get(c, "/domains").await,
            daemon_get(c, "/networks").await,
            daemon_get(c, "/pools").await,
        ) else {
            continue; // offline: show nothing, prune nothing
        };
        answered.push(c.id.clone());
        for d in doms.as_array().into_iter().flatten() {
            let name = d["name"].as_str().unwrap_or("").to_string();
            seen.push((Kind::Vm, c.id.clone(), String::new(), name.clone()));
            out.push(json!({
                "kind": "vm", "conn": c.id, "conn_name": c.name, "name": name,
                "uuid": d["uuid"], "title": d["title"],
                "state": d["state"], "vcpus": d["vcpus"], "memory_mb": d["memory_mb"],
            }));
        }
        for n in nets.as_array().into_iter().flatten() {
            let name = n["name"].as_str().unwrap_or("").to_string();
            seen.push((Kind::Network, c.id.clone(), String::new(), name.clone()));
            out.push(json!({
                "kind": "network", "conn": c.id, "conn_name": c.name, "name": name,
                "uuid": n["uuid"], "title": n["title"],
                "state": if n["active"].as_bool() == Some(true) { "active" } else { "inactive" },
                "forward": n["forward"], "bridge": n["bridge"], "ip": n["ip"],
            }));
        }
        for p in pools.as_array().into_iter().flatten() {
            let pool = p["name"].as_str().unwrap_or("").to_string();
            out.push(json!({
                "kind": "pool", "conn": c.id, "conn_name": c.name, "name": pool,
                "uuid": p["uuid"], "title": p["title"],
                "state": if p["active"].as_bool() == Some(true) { "active" } else { "inactive" },
                "capacity": p["capacity"], "allocation": p["allocation"],
                "available": p["available"],
            }));
            let Ok(vols) = daemon_get(c, &format!("/pools/{}/volumes", enc(&pool))).await else {
                continue;
            };
            for v in vols.as_array().into_iter().flatten() {
                let name = v["name"].as_str().unwrap_or("").to_string();
                seen.push((Kind::Volume, c.id.clone(), pool.clone(), name.clone()));
                out.push(json!({
                    "kind": "volume", "conn": c.id, "conn_name": c.name, "pool": pool,
                    "name": name, "title": v["title"], "capacity": v["capacity"], "path": v["path"],
                }));
            }
        }
    }

    let mut store = state.write();
    let now = crate::history::now_ms();
    let before = store.resources.len();
    store.resources.retain(|r| {
        !answered.contains(&r.conn)
            || now.saturating_sub(r.created_ms) < PRUNE_GRACE_MS
            || seen
                .iter()
                .any(|(k, c, p, n)| *k == r.kind && *c == r.conn && *p == r.pool && *n == r.name)
    });
    if store.resources.len() != before {
        let _ = store.save(&state.data_dir);
    }
    let default = store.default_owner().unwrap_or_default();
    for x in &mut out {
        let kind = match x["kind"].as_str() {
            Some("vm") => Kind::Vm,
            Some("network") => Kind::Network,
            Some("volume") => Kind::Volume,
            _ => continue,
        };
        let (conn, pool, name) = (
            x["conn"].as_str().unwrap_or(""),
            x["pool"].as_str().unwrap_or(""),
            x["name"].as_str().unwrap_or(""),
        );
        let r = store
            .resources
            .iter()
            .find(|r| r.kind == kind && r.conn == conn && r.pool == pool && r.name == name);
        let owner = r
            .map(|r| r.owner.clone())
            .unwrap_or_else(|| default.clone());
        x["owner"] = json!(owner);
        x["owner_name"] = json!(store.user_by_id(&owner).map(|u| u.username.clone()));
        x["suspended"] = json!(r.is_some_and(|r| r.suspended));
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
struct SetResource {
    kind: Kind,
    conn: String,
    #[serde(default)]
    pool: String,
    name: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    suspended: Option<bool>,
}

/// Hand a resource to another user, or suspend or restore their access to it.
async fn set_resource(
    State(state): State<AppState>,
    Json(req): Json<SetResource>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    if store.connection(&req.conn).is_none() {
        return Err(ApiError::not_found("no such connection"));
    }
    if let Some(o) = &req.owner {
        if store.user_by_id(o).is_none() {
            return Err(ApiError::bad_request("no such user"));
        }
    }
    let default = store.default_owner().unwrap_or_default();
    let idx = store.resources.iter().position(|r| {
        r.kind == req.kind && r.conn == req.conn && r.pool == req.pool && r.name == req.name
    });
    let idx = match idx {
        Some(i) => i,
        None => {
            store.resources.push(Resource {
                kind: req.kind,
                conn: req.conn.clone(),
                pool: req.pool.clone(),
                name: req.name.clone(),
                owner: default,
                suspended: false,
                path: String::new(),
                capacity: 0,
                blank: false,
                // Not a fresh create: prunable at once if it is gone.
                created_ms: 0,
            });
            store.resources.len() - 1
        }
    };
    let r = &mut store.resources[idx];
    if let Some(o) = req.owner {
        r.owner = o;
    }
    if let Some(s) = req.suspended {
        r.suspended = s;
    }
    let r = r.clone();
    store.save(&state.data_dir)?;
    Ok(Json(json!(r)))
}

// ---------------------------------------------------------------- usage

/// Every non-admin user's usage (summed over the daemons that answer) next
/// to their limits, for the admin dashboard. Read live, the same way the
/// quota check does, so the two can never disagree.
async fn usage(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (conns, users): (Vec<Connection>, Vec<crate::store::User>) = {
        let s = state.read();
        let users = s.users.iter().filter(|u| !s.is_admin(u)).cloned().collect();
        (s.connections.clone(), users)
    };
    let mut out = vec![];
    for u in &users {
        let mut total = crate::perm::Usage::default();
        for c in &conns {
            if !online(&state, &c.id) {
                continue;
            }
            if let Ok(x) = crate::guard::usage_on(&state, c, &u.id).await {
                total = total.add(x);
            }
        }
        out.push(json!({
            "id": u.id,
            "username": u.username,
            "quota": u.quota,
            "vms": total.vms,
            "vcpus": total.vcpus,
            "memory_mb": total.memory_mb,
            "storage_bytes": total.storage_bytes,
            "networks": total.networks,
            "forwards": total.forwards,
        }));
    }
    Ok(Json(json!(out)))
}

// ---------------------------------------------------------------- IPs

#[derive(Deserialize)]
struct IpsQuery {
    #[serde(default)]
    user: String,
}

/// Every address reservation on every managed network, with the guest that
/// holds it and that guest's owner. `?user=` narrows to one owner's guests.
/// Editing goes through the network's own `hosts` routes.
async fn ips(
    State(state): State<AppState>,
    Query(q): Query<IpsQuery>,
) -> Result<Json<Value>, ApiError> {
    let conns: Vec<Connection> = state.read().connections.clone();
    let mut out = vec![];
    for c in &conns {
        if !online(&state, &c.id) {
            continue;
        }
        let Ok(nets) = daemon_get(c, "/networks").await else {
            continue;
        };
        for n in nets.as_array().into_iter().flatten() {
            let net = n["name"].as_str().unwrap_or("");
            // A network is named by its UUID and keeps its nickname as a title;
            // without it the admin table showed a UUID for the network column.
            let net_title = n["title"].as_str().unwrap_or("");
            // A network this daemon does not serve (bridge mode) has none.
            let Ok(h) = daemon_get(c, &format!("/networks/{}/hosts", enc(net))).await else {
                continue;
            };
            let store = state.read();
            for r in h["hosts"].as_array().into_iter().flatten() {
                let vm = r["vm_name"].as_str().unwrap_or("");
                let owner = if vm.is_empty() {
                    String::new()
                } else {
                    store.owner_of(Kind::Vm, &c.id, vm).unwrap_or_default()
                };
                if !q.user.is_empty() && owner != q.user {
                    continue;
                }
                let mut r = r.clone();
                r["conn"] = json!(c.id);
                r["conn_name"] = json!(c.name);
                r["network"] = json!(net);
                r["network_title"] = json!(net_title);
                r["owner"] = json!(owner);
                r["owner_name"] = json!(store.user_by_id(&owner).map(|u| u.username.clone()));
                out.push(r);
            }
        }
    }
    Ok(Json(json!(out)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_tree_paths_can_carry_grants() {
        for ok in [
            "/",
            "/nodes",
            "/nodes/c",
            "/nodes/c/vms",
            "/nodes/c/vms/web",
            "/nodes/c/bridges/br0",
        ] {
            assert!(super::valid_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/vms",
            "/nodes/c/other",
            "/nodes/c/vms/a/b",
            "/nodes//vms",
        ] {
            assert!(!super::valid_path(bad), "{bad}");
        }
    }
}
