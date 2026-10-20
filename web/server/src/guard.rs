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

/// How long a read against a daemon may take before it is abandoned.
///
/// A refused connection fails at once, but a host that accepts and then never
/// answers would otherwise hold the request open forever — and the admin panel
/// fans out over every daemon, so one black hole made the whole page hang. A
/// read that has not answered in this long is as good as down.
const DAEMON_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A GET against the daemon, as JSON. The proxy's own caller is checked;
/// this is the web server reading for itself.
pub async fn daemon_get(conn: &Connection, path: &str) -> Result<Value, ApiError> {
    let addr = crate::daemon::parse_url(&conn.url).map_err(ApiError::internal)?;
    let client = crate::daemon::http_client(conn).map_err(ApiError::internal)?;
    let resp = tokio::time::timeout(
        DAEMON_GET_TIMEOUT,
        client
            .get(format!("{}{path}", addr.base))
            .header("Authorization", format!("Bearer {}", conn.token))
            .send(),
    )
    .await
    .map_err(|_| ApiError::bad_request(format!("daemon did not answer {path} in time")))?
    .map_err(|e| ApiError::bad_request(format!("daemon error: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "daemon answered {} for {path}",
            resp.status()
        )));
    }
    let bytes = tokio::time::timeout(DAEMON_GET_TIMEOUT, resp.bytes())
        .await
        .map_err(|_| ApiError::bad_request(format!("daemon body timed out for {path}")))?
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

/// Whether a volume is ISO media, by its name.
///
/// An ISO is shared media: it belongs to nobody, so it costs no one storage,
/// and any user who may read the pool may upload or download one. Anything
/// else (a disk image) is the uploader's own, and counts against their storage
/// like any other volume.
pub fn is_iso(name: &str) -> bool {
    name.trim().to_ascii_lowercase().ends_with(".iso")
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
    let (vms, nets, vols): (Vec<String>, Vec<String>, Vec<Resource>) = {
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
    // Port forwards on the networks the owner has here. Read live, so a
    // forward added outside the app is counted too.
    for n in &nets {
        if let Ok(fw) = daemon_get(conn, &format!("/networks/{}/firewall", enc(n))).await {
            u.forwards += fw
                .get("forwards")
                .and_then(Value::as_array)
                .map(|a| a.len() as u64)
                .unwrap_or(0);
        }
    }
    u.storage_bytes = vols.iter().map(|r| r.capacity).sum();
    Ok(u)
}

/// Whether a stored quota applies to this owner at all.
///
/// Was this an admin? Then no quota they hold is ever enforced: the admin page
/// has always said "Administrators are not limited", and it no longer offers
/// them the fields, so a stored quota can only be a leftover. Before this, only
/// the *creating* admin was skipped (`if !admin`, where the actor is checked,
/// not the owner), so a guest owned by an admin was capped by the admin's own
/// quota -- which is the case this closes.
///
/// Pure and public so the rule is testable without an `AppState`.
pub fn quota_applies(is_owner_admin: bool, q: &perm::Quota) -> bool {
    !is_owner_admin && *q != perm::Quota::default()
}

/// Would `owner` stay within their quota if `delta` were added on `conn`?
pub async fn check_quota(
    state: &AppState,
    conn: &Connection,
    owner: &User,
    delta: Usage,
) -> Result<(), ApiError> {
    let q = owner.quota.clone();
    // Checked on the owner rather than the caller: ownership is what a quota
    // limits, and administration travels with the account (a group can grant
    // it), so it is re-read here instead of trusting a caller's idea.
    if !quota_applies(state.read().is_admin(owner), &q) {
        return Ok(());
    }
    let here = usage_on(state, conn, &owner.id).await?.add(delta);
    // Walked only for a pooled default with no override here: an override is
    // that daemon's own ceiling, so the pool never answers for it (`perm::
    // check_quota`), and walking every daemon would be a round trip per daemon
    // for nothing.
    let everywhere = if q.shared && !q.daemons.contains_key(&conn.id) {
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

/// The networks that attach to `bridge`, by their stored key. A bridge can be
/// shared: two `Mode::Bridge` networks may borrow the same operator bridge, so
/// every one of them is a way in, not just the first match.
fn networks_for_bridge(nets: &Value, bridge: &str) -> Vec<String> {
    nets.as_array()
        .into_iter()
        .flatten()
        .filter(|n| n.get("bridge").and_then(Value::as_str) == Some(bridge))
        .filter_map(|n| n.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Every bridge a guest attaches to must be a network the caller may use. An
/// unknown bridge is refused rather than allowed: it could be the host's own
/// LAN bridge, which no network here grants.
async fn check_network_bridges(
    state: &AppState,
    user: &User,
    conn: &Connection,
    bridges: &[String],
) -> Result<(), ApiError> {
    if bridges.is_empty() {
        return Ok(());
    }
    // One read answers for every bridge: the network list carries each
    // network's bridge, and a guest's NIC names the bridge, not the network.
    let nets = daemon_get(conn, "/networks").await?;
    let store = state.read();
    for bridge in bridges {
        let keys = networks_for_bridge(&nets, bridge);
        // A shared bridge is one attachment point: a grant on any network that
        // uses it is enough, and that network's firewall is what applies.
        if keys.is_empty() {
            return Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                format!("no network uses bridge '{bridge}': a guest cannot attach to it"),
            ));
        }
        if !keys
            .iter()
            .any(|key| store.can(user, &perm::net_path(&conn.id, key), "Network.Use"))
        {
            return Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                format!(
                    "you may not attach a guest to bridge '{bridge}': no network using it is yours"
                ),
            ));
        }
    }
    Ok(())
}

/// Everything a non-admin names in a guest definition must be theirs to use:
/// each network (`Network.Use`); each writable disk a volume they own; each
/// read-only disk or CD-ROM any volume in a pool they can read (shared media
/// like ISOs). A path that is no volume at all is refused.
///
/// `before` is the definition the guest has now, when one is being re-saved:
/// a network or bridge it already has needs no new grant, or a grant revoked
/// since would lock an owner out of their own guest. `None` (a create) checks
/// every one. Disks are always checked: their ownership is what a save could
/// change.
async fn check_domain_refs(
    state: &AppState,
    user: &User,
    conn: &Connection,
    spec: &xmlcheck::DomainSpec,
    before: Option<&xmlcheck::DomainSpec>,
) -> Result<(), ApiError> {
    let denied = |m: String| ApiError::new(axum::http::StatusCode::FORBIDDEN, m);
    let empty: Vec<String> = Vec::new();
    let (have_nets, have_bridges) = match before {
        Some(b) => (&b.networks, &b.bridges),
        None => (&empty, &empty),
    };
    for net in spec.networks.iter().filter(|n| !have_nets.contains(*n)) {
        if !state
            .read()
            .can(user, &perm::net_path(&conn.id, net), "Network.Use")
        {
            return Err(denied(format!("you may not use network '{net}'")));
        }
    }
    let bridges: Vec<String> = spec
        .bridges
        .iter()
        .filter(|b| !have_bridges.contains(b))
        .cloned()
        .collect();
    check_network_bridges(state, user, conn, &bridges).await?;
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
        Special::None | Special::ListVms | Special::ListNetworks | Special::ListPools => Ok(None),
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
                check_domain_refs(state, user, conn, &spec, None).await?;
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
            // A save only has to justify what it adds: a network or bridge the
            // guest already has was allowed when it was made. The current
            // definition is read for that; if the read or parse fails, check
            // everything -- refusing too much is the safe failure, silently
            // allowing a new bridge is not.
            let before = match daemon_get(conn, &format!("/domains/{}/xml", enc(vm))).await {
                Ok(v) => {
                    xmlcheck::check_domain(v.get("xml").and_then(Value::as_str).unwrap_or("")).ok()
                }
                Err(_) => None,
            };
            check_domain_refs(state, user, conn, &spec, before.as_ref()).await?;
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
        // Ownership and grants follow the guest to its new name once the
        // daemon accepts it (see `after`).
        Special::RenameVm { .. } => Ok(None),
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
        Special::PutNetworkFirewall { net } => {
            if admin {
                return Ok(None);
            }
            let req: Value =
                serde_json::from_slice(body).map_err(|_| ApiError::bad_request("bad JSON"))?;
            let want = req
                .get("forwards")
                .and_then(Value::as_array)
                .map(|a| a.len() as u64)
                .unwrap_or(0);
            // Only growth counts: take the forwards already on this network
            // away from what the request would leave behind.
            let cur = daemon_get(conn, &format!("/networks/{}/firewall", enc(net))).await?;
            let have = cur
                .get("forwards")
                .and_then(Value::as_array)
                .map(|a| a.len() as u64)
                .unwrap_or(0);
            let grow = want.saturating_sub(have);
            if grow > 0 {
                check_quota(
                    state,
                    conn,
                    user,
                    Usage {
                        forwards: grow,
                        ..Usage::default()
                    },
                )
                .await?;
            }
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
        Special::Fetch { pool } => {
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
            // ISO media is shared: it belongs to nobody, so it costs no one
            // storage and any user who may read the pool may download one.
            // Nothing is recorded; `owner_of` then falls back to the first
            // administrator, exactly as for an ISO copied into the pool by
            // hand.
            if is_iso(&name) {
                return Ok(None);
            }
            // The size is unknown until the download ends, so a fetch cannot
            // be held to a storage quota up front: admins only, for anyone
            // with one. The ceiling is the one in force for *this* daemon, so
            // a per-daemon override is honoured rather than the datacenter
            // default alone.
            if !admin && perm::storage_limit(&user.quota, &conn.id).is_some() {
                return Err(ApiError::bad_request(
                    "fetching from a URL is not available with a storage limit; upload the file instead",
                ));
            }
            let mut r = record(Kind::Volume, pool, &name, 0);
            r.blank = false;
            Ok(Some(r))
        }
        Special::DeleteVm { .. } | Special::DeleteNetwork { .. } => Ok(None),
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
        Special::CreateVm
        | Special::CreateNetwork
        | Special::CreateVolume { .. }
        | Special::Fetch { .. } => {
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
        Special::RenameVm { vm } => {
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
            if let Some(r) = store.resource_mut(Kind::Vm, conn, vm) {
                r.name = to.clone();
            }
            let (from, dest) = (perm::vm_path(conn, vm), perm::vm_path(conn, &to));
            for e in store.acl.iter_mut().filter(|e| e.path == from) {
                e.path = dest.clone();
            }
        }
        Special::DeleteVm { vm: name } => {
            forget(&mut store, Kind::Vm, conn, "", name);
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
        Kind::Network => perm::net_path(conn, name),
        Kind::Volume => return,
    };
    // A later resource of the same name must not inherit this one's grants.
    store.acl.retain(|e| e.path != path);
}

#[cfg(test)]
mod tests {
    use super::{networks_for_bridge, quota_applies};
    use crate::perm::Quota;

    fn some_quota() -> Quota {
        Quota {
            default: crate::perm::Limit {
                vms: Some(1),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_stored_quota_does_not_apply_to_an_administrator() {
        // The admin page says "Administrators are not limited", and no longer
        // offers them the fields. Enforcing a leftover quota against an admin
        // is what this prevents -- previously only the *creating* admin was
        // skipped, so a guest owned by an admin was capped by the admin's own
        // quota.
        let q = some_quota();
        assert!(
            !quota_applies(true, &q),
            "an admin's stored quota must not be enforced"
        );
        assert!(
            !quota_applies(true, &Quota::default()),
            "and an absent one is equally not enforced"
        );
    }

    #[test]
    fn a_bridge_maps_to_every_network_that_uses_it() {
        // Two operator-bridge networks may borrow one bridge, so the caller's
        // grant on either is enough -- and a bridge no network owns maps to
        // nothing, which is what makes an arbitrary host bridge unreachable.
        let nets = serde_json::json!([
            { "name": "first", "bridge": "br0" },
            { "name": "second", "bridge": "br0" },
            { "name": "third", "bridge": "vbr0123456789ab" },
        ]);
        assert_eq!(networks_for_bridge(&nets, "br0"), ["first", "second"]);
        assert_eq!(networks_for_bridge(&nets, "vbr0123456789ab"), ["third"]);
        assert!(
            networks_for_bridge(&nets, "eth0").is_empty(),
            "an unknown bridge belongs to no network"
        );
        assert!(networks_for_bridge(&serde_json::json!({}), "br0").is_empty());
    }

    #[test]
    fn a_quota_still_applies_to_a_regular_user() {
        assert!(
            quota_applies(false, &some_quota()),
            "a non-admin with limits is capped"
        );
        // No limits means no check at all, admin or not.
        assert!(
            !quota_applies(false, &Quota::default()),
            "an empty quota is not a limit of zero"
        );
    }
}
