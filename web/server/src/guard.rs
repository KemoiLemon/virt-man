//! The checks the proxy makes that need more than the route: filtering lists
//! to what the caller may see, checking what a non-admin asks libvirt to
//! define, applying quotas, and recording who created what.
//!
//! Administrators skip the body checks and quotas: they have the whole of
//! libvirt, as before. Everyone else gets the forms' subset (`xmlcheck`),
//! within their limits, on resources they may use.

use serde_json::Value;

use crate::api::ApiError;
use crate::perm::{self, Kind, Resource, Usage};
use crate::routes::Special;
use crate::state::AppState;
use crate::store::{Connection, User};
use crate::xmlcheck;

/// A GET against the daemon, as JSON. The proxy's own caller is checked;
/// this is the web server reading for itself.
pub async fn daemon_get(conn: &Connection, path: &str) -> Result<Value, ApiError> {
    let addr = crate::daemon::parse_url(&conn.url).map_err(ApiError::internal)?;
    let client = crate::daemon::http_client(conn).map_err(ApiError::internal)?;
    let resp = client
        .get(format!("{}{path}", addr.base))
        .header("Authorization", format!("Bearer {}", conn.token))
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("daemon error: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "daemon answered {} for {path}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::internal(format!("daemon body: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::internal(format!("daemon JSON: {e}")))
}

/// Whether `s` is a UUID in libvirt's canonical 8-4-4-4-12 hex form -- the
/// same test the daemon applies, so the two agree on what counts as one.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Keep the list entries the caller may see, and say who owns each so the
/// UI can show it. Used for guests and networks.
pub fn filter_list(
    state: &AppState,
    user: &User,
    conn: &str,
    kind: Kind,
    body: &[u8],
) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let store = state.read();
    let (path_of, audit): (fn(&str, &str) -> String, &str) = match kind {
        Kind::Vm => (perm::vm_path, "VM.Audit"),
        Kind::Ct => (perm::ct_path, "CT.Audit"),
        Kind::Network => (perm::net_path, "Network.Audit"),
        Kind::Volume => return None,
    };
    let arr = v.as_array_mut()?;
    arr.retain(|x| {
        let name = x.get("name").and_then(Value::as_str).unwrap_or("");
        store.can(user, &path_of(conn, name), audit)
    });
    for x in arr.iter_mut() {
        let name = x
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let owner = store.owner_of(kind, conn, &name).unwrap_or_default();
        let r = store.resource(kind, conn, &name);
        x["owner"] = owner.clone().into();
        x["owner_name"] = store
            .user_by_id(&owner)
            .map(|u| u.username.clone())
            .unwrap_or_default()
            .into();
        x["suspended"] = r.is_some_and(|r| r.suspended).into();
        x["mine"] = (owner == user.id).into();
    }
    serde_json::to_vec(&v).ok()
}

/// Pools the caller may see.
pub fn filter_pools(state: &AppState, user: &User, conn: &str, body: &[u8]) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let store = state.read();
    v.as_array_mut()?.retain(|x| {
        let name = x.get("name").and_then(Value::as_str).unwrap_or("");
        store.can(user, &perm::pool_path(conn, name), "Datastore.Audit")
    });
    serde_json::to_vec(&v).ok()
}

/// Bridges the caller may attach a network to.
pub fn filter_bridges(state: &AppState, user: &User, conn: &str, body: &[u8]) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let store = state.read();
    v.as_array_mut()?.retain(|x| {
        let name = x.as_str().unwrap_or("");
        store.can(user, &perm::bridge_path(conn, name), "Bridge.Use")
    });
    serde_json::to_vec(&v).ok()
}

/// Volumes in a pool: everyone who may read the pool sees all of them (ISOs
/// are shared media), marked with their owner so the UI knows which it may
/// delete.
pub fn annotate_volumes(
    state: &AppState,
    user: &User,
    conn: &str,
    pool: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let store = state.read();
    for x in v.as_array_mut()? {
        let name = x
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let rec = store
            .resources
            .iter()
            .find(|r| r.kind == Kind::Volume && r.conn == conn && r.pool == pool && r.name == name);
        let owner = rec
            .map(|r| r.owner.clone())
            .or_else(|| store.default_owner())
            .unwrap_or_default();
        x["mine"] = (owner == user.id).into();
        // Whether it may be attached as qcow2 (made empty here, so it has no
        // backing file); anything else only as raw. Lets the create form offer
        // only disks the define will accept, instead of failing at the end.
        x["blank"] = rec.is_some_and(|r| r.blank).into();
        // In use by a guest already: `check_domain_refs` does not stop two
        // guests sharing a writable disk, so the form has to say so.
        x["owner_name"] = store
            .user_by_id(&owner)
            .map(|u| u.username.clone())
            .unwrap_or_default()
            .into();
    }
    serde_json::to_vec(&v).ok()
}

/// A user's usage on one daemon, from the daemon's own figures: everything
/// they own, running or not. Read live rather than tracked, so a guest
/// resized or deleted outside the app is counted as it is.
pub async fn usage_on(state: &AppState, conn: &Connection, owner: &str) -> Result<Usage, ApiError> {
    let (vms, cts, nets, vols): (Vec<String>, Vec<String>, Vec<String>, Vec<Resource>) = {
        let store = state.read();
        let is_default = store.default_owner().as_deref() == Some(owner);
        let mine = |k: Kind| -> Vec<String> {
            store
                .resources
                .iter()
                .filter(|r| r.kind == k && r.conn == conn.id && r.owner == owner)
                .map(|r| r.name.clone())
                .collect()
        };
        if is_default {
            // The default owner holds everything unrecorded; an admin has no
            // quota worth counting, so skip the walk.
            return Ok(Usage::default());
        }
        (
            mine(Kind::Vm),
            mine(Kind::Ct),
            mine(Kind::Network),
            store
                .resources
                .iter()
                .filter(|r| r.kind == Kind::Volume && r.conn == conn.id && r.owner == owner)
                .cloned()
                .collect(),
        )
    };
    let mut u = Usage {
        networks: nets.len() as u64,
        ..Usage::default()
    };
    if !vms.is_empty() {
        let list = daemon_get(conn, "/domains").await?;
        for d in list.as_array().into_iter().flatten() {
            let name = d.get("name").and_then(Value::as_str).unwrap_or("");
            if vms.iter().any(|v| v == name) {
                u.vms += 1;
                u.vcpus += d.get("vcpus").and_then(Value::as_u64).unwrap_or(0);
                u.memory_mb += d.get("memory_mb").and_then(Value::as_u64).unwrap_or(0);
            }
        }
    }
    // ponytail: a container counts against the VM, vCPU and memory limits;
    // separate container limits when someone needs to set them apart.
    if !cts.is_empty() {
        let list = daemon_get(conn, "/containers").await?;
        for d in list.as_array().into_iter().flatten() {
            let name = d.get("name").and_then(Value::as_str).unwrap_or("");
            if cts.iter().any(|v| v == name) {
                u.vms += 1;
                u.vcpus += d.get("vcpus").and_then(Value::as_u64).unwrap_or(0);
                u.memory_mb += d.get("memory_mb").and_then(Value::as_u64).unwrap_or(0);
            }
        }
    }
    u.storage_bytes = vols.iter().map(|r| r.capacity).sum();
    Ok(u)
}

/// Would `owner` stay within their quota if `delta` were added on `conn`?
pub async fn check_quota(
    state: &AppState,
    conn: &Connection,
    owner: &User,
    delta: Usage,
) -> Result<(), ApiError> {
    let q = owner.quota.clone();
    if q == perm::Quota::default() {
        return Ok(());
    }
    let here = usage_on(state, conn, &owner.id).await?.add(delta);
    let everywhere = if q.shared {
        let others: Vec<Connection> = state
            .read()
            .connections
            .iter()
            .filter(|c| c.id != conn.id)
            .cloned()
            .collect();
        let mut total = here;
        for c in &others {
            // An offline daemon cannot be counted; its last known usage is
            // not kept, so count what is recorded without live figures.
            let u = usage_on(state, c, &owner.id).await.unwrap_or_default();
            total = total.add(u);
        }
        total
    } else {
        here
    };
    perm::check_quota(&q, &conn.id, &conn.name, &here, &everywhere, &delta)
        .map_err(|e| ApiError::new(axum::http::StatusCode::FORBIDDEN, e))
}

/// Everything a non-admin names in a guest definition must be theirs to use:
/// each network (`Network.Use`); each writable disk a volume they own; each
/// read-only disk or CD-ROM any volume in a pool they can read (shared media
/// like ISOs). A path that is no volume at all is refused.
async fn check_domain_refs(
    state: &AppState,
    user: &User,
    conn: &Connection,
    spec: &xmlcheck::DomainSpec,
) -> Result<(), ApiError> {
    let denied = |m: String| ApiError::new(axum::http::StatusCode::FORBIDDEN, m);
    for net in &spec.networks {
        if !state
            .read()
            .can(user, &perm::net_path(&conn.id, net), "Network.Use")
        {
            return Err(denied(format!("you may not use network '{net}'")));
        }
    }
    if spec.disks.is_empty() {
        return Ok(());
    }
    // Resolve paths through the volume API, in the pools the user may read,
    // so a path outside every pool (or in one they cannot see) matches
    // nothing.
    let pools = daemon_get(conn, "/pools").await?;
    let mut found: Vec<(String, String, String)> = vec![]; // (pool, vol, path)
    for pool in names(&pools) {
        if !state
            .read()
            .can(user, &perm::pool_path(&conn.id, &pool), "Datastore.Audit")
        {
            continue;
        }
        let vols = daemon_get(conn, &format!("/pools/{}/volumes", enc(&pool))).await?;
        for v in vols.as_array().into_iter().flatten() {
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let path = v
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            found.push((pool.clone(), name, path));
        }
    }
    let store = state.read();
    let record = |pool: &str, vol: &str| {
        store.resources.iter().find(|r| {
            r.kind == Kind::Volume && r.conn == conn.id && r.pool == pool && r.name == vol
        })
    };
    for d in &spec.disks {
        let hit = match &d.source {
            xmlcheck::DiskSource::Path(p) => found.iter().find(|(_, _, x)| x == p),
            xmlcheck::DiskSource::Volume { pool, volume } => {
                found.iter().find(|(p, v, _)| p == pool && v == volume)
            }
        };
        let Some((pool, vol, _)) = hit else {
            let what = match &d.source {
                xmlcheck::DiskSource::Path(p) => p.clone(),
                xmlcheck::DiskSource::Volume { pool, volume } => format!("{pool}/{volume}"),
            };
            return Err(denied(format!("'{what}' is not a volume you may use")));
        };
        let r = record(pool, vol);
        if d.writable && r.is_none_or(|r| r.owner != user.id) {
            return Err(denied(format!(
                "'{vol}' is not yours: attach it read-only, or use a disk you created"
            )));
        }
        // A qcow2 header can name any host file as its backing store and
        // qemu opens it. Only a volume made empty by the volume API is known
        // to have none; anything else is attached as raw, which qemu does
        // not interpret.
        if d.format != "raw" && !r.is_some_and(|r| r.blank) {
            return Err(ApiError::bad_request(format!(
                "'{vol}' can only be attached as raw: only disks created here may be qcow2"
            )));
        }
    }
    Ok(())
}

/// What a non-admin's request is allowed to do, decided before it reaches
/// the daemon. Returns the resource to record once the daemon accepts it.
pub async fn before(
    state: &AppState,
    user: &User,
    conn: &Connection,
    special: &Special,
    body: &[u8],
) -> Result<Option<Resource>, ApiError> {
    let admin = state.read().is_admin(user);
    let text = || std::str::from_utf8(body).map_err(|_| ApiError::bad_request("body is not UTF-8"));
    let record = |kind, pool: &str, name: &str, capacity| Resource {
        kind,
        conn: conn.id.clone(),
        pool: pool.to_string(),
        name: name.to_string(),
        owner: user.id.clone(),
        suspended: false,
        path: String::new(),
        capacity,
        blank: kind == Kind::Volume,
        created_ms: crate::history::now_ms(),
    };
    let owner_of = |kind, name: &str| state.read().owner_of(kind, &conn.id, name);
    let owner_user = |id: Option<String>| -> Option<User> {
        id.and_then(|id| state.read().user_by_id(&id).cloned())
    };
    match special {
        Special::None
        | Special::ListVms
        | Special::ListCts
        | Special::ListNetworks
        | Special::ListPools => Ok(None),
        Special::ListVolumes { .. } | Special::ListBridges => Ok(None),
        Special::AdminOnly => {
            if admin {
                Ok(None)
            } else {
                Err(ApiError::new(
                    axum::http::StatusCode::FORBIDDEN,
                    "only an administrator can do that: it affects the whole server, not just your resources",
                ))
            }
        }
        Special::CreateVm => {
            let xml = text()?;
            // The body is parsed for its name, size and references either
            // way; only a non-admin is held to the allowlist.
            let spec = match xmlcheck::check_domain(xml) {
                Ok(s) => s,
                Err(e) if !admin => return Err(ApiError::bad_request(e)),
                Err(_) => return Ok(None),
            };
            if !admin {
                check_domain_refs(state, user, conn, &spec).await?;
                check_quota(
                    state,
                    conn,
                    user,
                    Usage {
                        vms: 1,
                        vcpus: spec.vcpus,
                        memory_mb: spec.memory_mb,
                        ..Usage::default()
                    },
                )
                .await?;
            }
            Ok(Some(record(Kind::Vm, "", &spec.name, 0)))
        }
        Special::PutVmXml { vm } => {
            if admin {
                return Ok(None);
            }
            let spec = xmlcheck::check_domain(text()?).map_err(ApiError::bad_request)?;
            if &spec.name != vm {
                // Defining under another name creates a second guest (or
                // replaces someone else's); renaming has its own route.
                return Err(ApiError::bad_request(
                    "the definition names a different guest",
                ));
            }
            check_domain_refs(state, user, conn, &spec).await?;
            // Growth counts against the owner's quota: take the current size
            // away and add the new one.
            let owner = owner_user(owner_of(Kind::Vm, vm)).unwrap_or_else(|| user.clone());
            // `/domains/{vm}` carries no sizes; the list does.
            let list = daemon_get(conn, "/domains").await?;
            let cur = list
                .as_array()
                .into_iter()
                .flatten()
                .find(|d| d.get("name").and_then(Value::as_str) == Some(vm.as_str()));
            let size = |k: &str| {
                cur.and_then(|d| d.get(k))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let (old_cpu, old_mem) = (size("vcpus"), size("memory_mb"));
            let grow = Usage {
                vcpus: spec.vcpus.saturating_sub(old_cpu),
                memory_mb: spec.memory_mb.saturating_sub(old_mem),
                ..Usage::default()
            };
            if grow != Usage::default() {
                check_quota(state, conn, &owner, grow).await?;
            }
            Ok(None)
        }
        Special::CreateCt => {
            let req: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let privileged = req
                .get("privileged")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let cts = perm::node_path(&conn.id) + "/containers";
            if privileged && !state.read().can(user, &cts, "CT.Privileged") {
                return Err(forbidden(format!(
                    "a privileged container runs as the host's root, which needs CT.Privileged on {}. \
                     Create it unprivileged, or ask an administrator for the CTPrivileged role.",
                    describe(state, &cts)
                )));
            }
            let s = |p: &[&str]| {
                p.iter()
                    .try_fold(&req, |v, k| v.get(k))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let (tpool, rpool) = (s(&["template", "pool"]), s(&["rootfs", "pool"]));
            let size_gb = req
                .pointer("/rootfs/size_gb")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if !admin {
                // Scoped: a read guard held across the quota's await would
                // make the proxy future non-Send.
                {
                    let store = state.read();
                    if !store.can(user, &perm::pool_path(&conn.id, &tpool), "Datastore.Audit") {
                        return Err(forbidden(format!(
                            "you may not use templates in pool '{tpool}'"
                        )));
                    }
                    if !store.can(
                        user,
                        &perm::pool_path(&conn.id, &rpool),
                        "Datastore.AllocateSpace",
                    ) {
                        return Err(forbidden(format!(
                            "you may not create a container's disk in pool '{rpool}'"
                        )));
                    }
                    for n in req
                        .get("networks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !store.can(user, &perm::net_path(&conn.id, n), "Network.Use") {
                            return Err(forbidden(format!("you may not use network '{n}'")));
                        }
                    }
                }
                let num = |k: &str| req.get(k).and_then(Value::as_u64).unwrap_or(0);
                check_quota(
                    state,
                    conn,
                    user,
                    Usage {
                        vms: 1,
                        vcpus: num("vcpus"),
                        memory_mb: num("memory_mb"),
                        storage_bytes: size_gb << 30,
                        ..Usage::default()
                    },
                )
                .await?;
            }
            // Named by the daemon (a new UUID); `after` fills it in and
            // records the rootfs volume too, from the same answer.
            let mut r = record(Kind::Ct, &rpool, "", size_gb << 30);
            r.blank = false;
            Ok(Some(r))
        }
        Special::PutCtXml { ct } => {
            if admin {
                return Ok(None);
            }
            let spec = xmlcheck::check_container(text()?).map_err(ApiError::bad_request)?;
            if &spec.name != ct {
                return Err(ApiError::bad_request(
                    "the definition names a different container",
                ));
            }
            let cur = daemon_get(conn, &format!("/containers/{}/xml", enc(ct))).await?;
            let cur =
                xmlcheck::check_container(cur.get("xml").and_then(Value::as_str).unwrap_or(""))
                    .map_err(ApiError::internal)?;
            // The idmap *is* the isolation: dropping it, or pointing it at
            // another range (uid 0, or someone else's container), makes the
            // container's root someone it should not be.
            if (spec.privileged || spec.idmap != cur.idmap)
                && !state
                    .read()
                    .can(user, &perm::ct_path(&conn.id, ct), "CT.Privileged")
            {
                return Err(forbidden(
                    "changing a container's user-namespace mapping, or running it privileged, \
                     needs CT.Privileged on it; ask an administrator"
                        .to_string(),
                ));
            }
            for n in &spec.networks {
                if !cur.networks.contains(n)
                    && !state
                        .read()
                        .can(user, &perm::net_path(&conn.id, n), "Network.Use")
                {
                    return Err(forbidden(format!("you may not use network '{n}'")));
                }
            }
            let grow = Usage {
                vcpus: spec.vcpus.saturating_sub(cur.vcpus),
                memory_mb: spec.memory_mb.saturating_sub(cur.memory_mb),
                ..Usage::default()
            };
            if grow != Usage::default() {
                let owner = owner_user(owner_of(Kind::Ct, ct)).unwrap_or_else(|| user.clone());
                check_quota(state, conn, &owner, grow).await?;
            }
            Ok(None)
        }
        Special::PutCtHardware { ct } => {
            if admin {
                return Ok(None);
            }
            let want: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let cur = daemon_get(conn, &format!("/containers/{}/hardware", enc(ct))).await?;
            let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
            // Only growth counts, and a bigger disk is storage the owner holds.
            let grow = Usage {
                vcpus: n(&want, "vcpus").saturating_sub(n(&cur, "vcpus")),
                memory_mb: n(&want, "memory_mb").saturating_sub(n(&cur, "memory_mb")),
                storage_bytes: n(&want, "disk_gb").saturating_sub(n(&cur, "disk_gb")) << 30,
                ..Usage::default()
            };
            if grow != Usage::default() {
                let owner = owner_user(owner_of(Kind::Ct, ct)).unwrap_or_else(|| user.clone());
                check_quota(state, conn, &owner, grow).await?;
            }
            Ok(None)
        }
        // Ownership and grants follow the guest to its new name once the
        // daemon accepts it (see `after`).
        Special::RenameVm { .. } | Special::RenameCt { .. } => Ok(None),
        Special::AddNic { .. } => {
            let req: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let net = req.get("network").and_then(Value::as_str).unwrap_or("");
            if !state
                .read()
                .can(user, &perm::net_path(&conn.id, net), "Network.Use")
            {
                return Err(ApiError::new(
                    axum::http::StatusCode::FORBIDDEN,
                    format!("you may not use network '{net}'"),
                ));
            }
            Ok(None)
        }
        Special::CreateNetwork => {
            let spec = match xmlcheck::check_network(text()?) {
                Ok(s) => s,
                Err(e) if !admin => return Err(ApiError::bad_request(e)),
                Err(_) => return Ok(None),
            };
            if !admin {
                check_bridge(state, user, &conn.id, &spec)?;
                check_quota(
                    state,
                    conn,
                    user,
                    Usage {
                        networks: 1,
                        ..Usage::default()
                    },
                )
                .await?;
            }
            Ok(Some(record(Kind::Network, "", &spec.name, 0)))
        }
        Special::PutNetworkXml { net } => {
            if admin {
                return Ok(None);
            }
            let spec = xmlcheck::check_network(text()?).map_err(ApiError::bad_request)?;
            if &spec.name != net {
                return Err(ApiError::bad_request(
                    "the definition names a different network",
                ));
            }
            check_bridge(state, user, &conn.id, &spec)?;
            Ok(None)
        }
        Special::CreateVolume { pool } => {
            let req: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let xml = req.get("xml").and_then(Value::as_str).unwrap_or("");
            let spec = match xmlcheck::check_volume(xml) {
                Ok(s) => s,
                Err(e) if !admin => return Err(ApiError::bad_request(e)),
                Err(_) => return Ok(None),
            };
            if !admin {
                check_quota(
                    state,
                    conn,
                    user,
                    Usage {
                        storage_bytes: spec.capacity,
                        ..Usage::default()
                    },
                )
                .await?;
            }
            Ok(Some(record(Kind::Volume, pool, &spec.name, spec.capacity)))
        }
        Special::Fetch { pool } | Special::FetchTemplate { pool } => {
            // The size is unknown until the download ends, so a fetch cannot
            // be held to a storage quota up front: admins only for anyone
            // with one. Without a quota, the fetched volume is theirs.
            if !admin && user.quota.default.storage_gb.is_some() {
                return Err(ApiError::bad_request(
                    "fetching from a URL is not available with a storage limit; upload the file instead",
                ));
            }
            let req: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    req.get("url")
                        .and_then(Value::as_str)
                        .and_then(|u| u.split('?').next())
                        .and_then(|u| u.rsplit('/').next())
                        .map(crate::routes::decode)
                })
                .unwrap_or_default();
            let mut r = record(Kind::Volume, pool, &name, 0);
            r.blank = false;
            Ok(Some(r))
        }
        Special::DeleteVm { .. } | Special::DeleteCt { .. } | Special::DeleteNetwork { .. } => {
            Ok(None)
        }
        Special::DeleteVolume { pool, vol } => {
            if admin {
                return Ok(None);
            }
            let store = state.read();
            let mine = store.resources.iter().any(|r| {
                r.kind == Kind::Volume
                    && r.conn == conn.id
                    && &r.pool == pool
                    && &r.name == vol
                    && r.owner == user.id
            });
            if mine {
                Ok(None)
            } else {
                Err(ApiError::new(
                    axum::http::StatusCode::FORBIDDEN,
                    "you may only delete volumes you created",
                ))
            }
        }
    }
}

fn forbidden(m: String) -> ApiError {
    ApiError::new(axum::http::StatusCode::FORBIDDEN, m)
}

/// A path in words, for a refusal.
fn describe(state: &AppState, path: &str) -> String {
    let store = state.read();
    perm::describe_path(path, &|c| store.connection(c).map(|x| x.name.clone()))
}

/// A network on a host bridge puts guests straight onto that bridge's LAN,
/// so it needs the bridge granted (`Bridge.Use` on `/nodes/<conn>/bridges/<br>`).
/// A NAT or isolated network's bridge is its own, made by libvirt.
fn check_bridge(
    state: &AppState,
    user: &User,
    conn: &str,
    spec: &xmlcheck::NetworkSpec,
) -> Result<(), ApiError> {
    if spec.forward != "bridge" {
        return Ok(());
    }
    let br = spec.bridge.as_deref().unwrap_or("");
    if state
        .read()
        .can(user, &perm::bridge_path(conn, br), "Bridge.Use")
    {
        Ok(())
    } else {
        Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            format!("you may not use bridge '{br}'; ask an administrator"),
        ))
    }
}

/// Bookkeeping once the daemon has accepted a request.
pub fn after(
    state: &AppState,
    conn: &str,
    special: &Special,
    created: Option<Resource>,
    body: &[u8],
    response: &[u8],
) {
    let mut store = state.write();
    match special {
        Special::CreateCt => {
            let Some(r) = created else { return };
            let resp: Value = serde_json::from_slice(response).unwrap_or_default();
            let Some(name) = resp.get("name").and_then(Value::as_str) else {
                return;
            };
            // The rootfs is a volume the owner made: it counts against their
            // storage, and goes with the container (`DeleteCt`).
            let vol = Resource {
                kind: Kind::Volume,
                name: format!("ct-{name}.raw"),
                ..r.clone()
            };
            let ct = Resource {
                kind: Kind::Ct,
                pool: String::new(),
                name: name.to_string(),
                capacity: 0,
                ..r
            };
            store.resources.push(vol);
            store.resources.push(ct);
        }
        Special::CreateVm
        | Special::CreateNetwork
        | Special::CreateVolume { .. }
        | Special::Fetch { .. }
        | Special::FetchTemplate { .. } => {
            let Some(mut r) = created else { return };
            // Trust the daemon's name over ours where it reports one.
            if let Some(n) = serde_json::from_slice::<Value>(response)
                .ok()
                .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
                .filter(|n| !n.is_empty())
            {
                r.name = n;
            }
            if let Some(p) = serde_json::from_slice::<Value>(response)
                .ok()
                .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_string))
            {
                r.path = p;
            }
            store.resources.retain(|x| {
                !(x.kind == r.kind && x.conn == r.conn && x.pool == r.pool && x.name == r.name)
            });
            store.resources.push(r);
        }
        Special::PutCtHardware { ct } => {
            // A grown rootfs is more of the owner's storage: the quota sums
            // volume records, so the record must say the new size.
            let gb = serde_json::from_slice::<Value>(response)
                .ok()
                .and_then(|v| v.pointer("/hardware/disk_gb").and_then(Value::as_u64));
            let Some(gb) = gb else { return };
            let vol = format!("ct-{ct}.raw");
            for r in store.resources.iter_mut() {
                if r.kind == Kind::Volume && r.conn == conn && r.name == vol {
                    r.capacity = r.capacity.max(gb << 30);
                }
            }
        }
        Special::RenameVm { vm: old } | Special::RenameCt { ct: old } => {
            let (kind, path_of): (Kind, fn(&str, &str) -> String) = match special {
                Special::RenameCt { .. } => (Kind::Ct, perm::ct_path),
                _ => (Kind::Vm, perm::vm_path),
            };
            let vm = old;
            let to = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
            if to.is_empty() {
                return;
            }
            if let Some(r) = store.resource_mut(kind, conn, vm) {
                r.name = to.clone();
            }
            let (from, dest) = (path_of(conn, vm), path_of(conn, &to));
            for e in store.acl.iter_mut().filter(|e| e.path == from) {
                e.path = dest.clone();
            }
        }
        Special::DeleteVm { vm: name } | Special::DeleteCt { ct: name } => {
            let kind = match special {
                Special::DeleteCt { .. } => Kind::Ct,
                _ => Kind::Vm,
            };
            forget(&mut store, kind, conn, "", name);
            // Disks removed with the guest take their records with them, or
            // their size would still count against the owner's storage.
            let removed: Vec<String> = serde_json::from_slice::<Value>(response)
                .ok()
                .and_then(|v| v.get("removed").cloned())
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            store.resources.retain(|r| {
                !(r.kind == Kind::Volume
                    && r.conn == conn
                    && removed
                        .iter()
                        .any(|x| *x == r.path || *x == format!("{}/{}", r.pool, r.name)))
            });
        }
        Special::DeleteNetwork { net } => forget(&mut store, Kind::Network, conn, "", net),
        Special::DeleteVolume { pool, vol } => forget(&mut store, Kind::Volume, conn, pool, vol),
        _ => return,
    }
    if let Err(e) = store.save(&state.data_dir) {
        tracing::warn!("could not record ownership: {e}");
    }
}

fn forget(store: &mut crate::store::Store, kind: Kind, conn: &str, pool: &str, name: &str) {
    store
        .resources
        .retain(|r| !(r.kind == kind && r.conn == conn && r.pool == pool && r.name == name));
    let path = match kind {
        Kind::Vm => perm::vm_path(conn, name),
        Kind::Ct => perm::ct_path(conn, name),
        Kind::Network => perm::net_path(conn, name),
        Kind::Volume => return,
    };
    // A later resource of the same name must not inherit this one's grants.
    store.acl.retain(|e| e.path != path);
}
