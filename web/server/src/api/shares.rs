//! Owners sharing their own VMs and containers with other users, without an
//! administrator. A request names `kind` ("vm", the default, or "ct").
//!
//! This is a write to the ACL by a non-admin, so it is deliberately narrow:
//! only someone who may delete the VM (`VM.Allocate` on it -- its owner, or an
//! admin) may share it; only to a user, not a group; only on exactly that
//! VM's path, not propagating; and only one of the fixed `SHARE_ROLES`.
//! Nothing here can grant ownership, reach another resource, or hand out
//! anything that is not already a VM privilege the owner holds.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::perm::{ct_path, vm_path, AclEntry, Kind};
use crate::state::AppState;
use crate::store::{Store, User};

/// What an owner may hand out: use it, or use and configure it.
pub const SHARE_ROLES: [&str; 2] = ["VMUser", "VMAdmin"];
pub const CT_SHARE_ROLES: [&str; 2] = ["CTUser", "CTAdmin"];

/// Which of the two a request is about. The `vm` field names either.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ShareKind {
    #[default]
    Vm,
    Ct,
}

impl ShareKind {
    fn kind(self) -> Kind {
        match self {
            ShareKind::Vm => Kind::Vm,
            ShareKind::Ct => Kind::Ct,
        }
    }
    fn path(self, conn: &str, name: &str) -> String {
        match self {
            ShareKind::Vm => vm_path(conn, name),
            ShareKind::Ct => ct_path(conn, name),
        }
    }
    fn roles(self) -> &'static [&'static str; 2] {
        match self {
            ShareKind::Vm => &SHARE_ROLES,
            ShareKind::Ct => &CT_SHARE_ROLES,
        }
    }
    fn word(self) -> &'static str {
        match self {
            ShareKind::Vm => "VM",
            ShareKind::Ct => "container",
        }
    }
}

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/shares", get(list).put(add).delete(remove))
        // Who can be shared with: every account's name, nothing else.
        .route("/shares/users", get(people))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

#[derive(Deserialize)]
struct Target {
    conn: String,
    vm: String,
    #[serde(default)]
    kind: ShareKind,
}

#[derive(Deserialize)]
struct ShareReq {
    conn: String,
    vm: String,
    user: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    kind: ShareKind,
}

/// May `user` share this VM? Its owner or an administrator, asked directly.
///
/// Not "holds VM.Allocate here": the default User role grants VM.Allocate at
/// `/` (that is what lets users create VMs), and it propagates onto every VM
/// path, so that test let any user share anyone's VM. Ownership is the only
/// thing that makes a VM yours; a user it was shared with, even as VMAdmin,
/// is not its owner and so cannot share it onward. A suspended VM cannot be
/// shared by its owner either -- suspension is a hold on their use of it.
fn may_share(
    store: &Store,
    user: &User,
    kind: ShareKind,
    conn: &str,
    vm: &str,
) -> Result<(), ApiError> {
    if store.connection(conn).is_none() {
        return Err(ApiError::not_found("no such connection"));
    }
    if store.is_admin(user) {
        return Ok(());
    }
    let w = kind.word();
    let owns = store.owner_of(kind.kind(), conn, vm).as_deref() == Some(user.id.as_str());
    let suspended = store
        .resource(kind.kind(), conn, vm)
        .is_some_and(|r| r.suspended);
    if owns && suspended {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("{w} '{vm}' is suspended by an administrator; it cannot be shared until they restore it"),
        ));
    }
    if owns {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("only the owner of {w} '{vm}' (or an administrator) can share it"),
        ))
    }
}

/// A privileged container's console is a root shell on the host. Handing
/// that out is an administrator's decision, however the container came to be
/// privileged. Read from the daemon, which holds the only record of it.
async fn refuse_privileged(
    state: &AppState,
    user: &User,
    conn: &str,
    ct: &str,
) -> Result<(), ApiError> {
    let c = {
        let store = state.read();
        if store.is_admin(user) {
            return Ok(());
        }
        store
            .connection(conn)
            .cloned()
            .ok_or_else(|| ApiError::not_found("no such connection"))?
    };
    let v = crate::guard::daemon_get(&c, &format!("/containers/{}", crate::guard::enc(ct))).await?;
    if v.get("privileged").and_then(Value::as_bool) != Some(false) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!(
                "container '{ct}' is privileged (its root is the host's root), so only an administrator can share it"
            ),
        ));
    }
    Ok(())
}

fn shares_of(store: &Store, kind: ShareKind, conn: &str, vm: &str) -> Vec<Value> {
    let path = kind.path(conn, vm);
    store
        .acl
        .iter()
        .filter(|e| e.path == path && kind.roles().contains(&e.role.as_str()))
        .filter_map(|e| {
            let id = e.ugid.strip_prefix("user:")?;
            let u = store.user_by_id(id)?;
            Some(json!({ "user": u.id, "username": u.username, "role": e.role }))
        })
        .collect()
}

async fn list(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(t): Query<Target>,
) -> Result<Json<Value>, ApiError> {
    let store = state.read();
    may_share(&store, &user, t.kind, &t.conn, &t.vm)?;
    let owner = store
        .owner_of(t.kind.kind(), &t.conn, &t.vm)
        .unwrap_or_default();
    Ok(Json(json!({
        "owner": owner,
        "roles": t.kind.roles(),
        "shares": shares_of(&store, t.kind, &t.conn, &t.vm),
    })))
}

async fn people(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Json<Value> {
    // Names only: enough to pick someone, nothing about their access.
    let store = state.read();
    let v: Vec<Value> = store
        .users
        .iter()
        .filter(|u| u.id != user.id)
        .map(|u| json!({ "id": u.id, "username": u.username }))
        .collect();
    Json(json!(v))
}

async fn add(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<ShareReq>,
) -> Result<Json<Value>, ApiError> {
    let k = req.kind;
    if !k.roles().contains(&req.role.as_str()) {
        return Err(ApiError::bad_request(format!(
            "a {} can be shared as {} only",
            k.word(),
            k.roles().join(" or ")
        )));
    }
    may_share(&state.read(), &user, k, &req.conn, &req.vm)?;
    if k == ShareKind::Ct {
        refuse_privileged(&state, &user, &req.conn, &req.vm).await?;
    }
    let mut store = state.write();
    may_share(&store, &user, k, &req.conn, &req.vm)?;
    if store.user_by_id(&req.user).is_none() {
        return Err(ApiError::bad_request("no such user"));
    }
    let owner = store
        .owner_of(k.kind(), &req.conn, &req.vm)
        .unwrap_or_default();
    if req.user == owner {
        return Err(ApiError::bad_request(format!(
            "that user already owns this {}",
            k.word()
        )));
    }
    let path = k.path(&req.conn, &req.vm);
    let ugid = format!("user:{}", req.user);
    // One level per person: sharing again changes it rather than stacking.
    store
        .acl
        .retain(|e| !(e.path == path && e.ugid == ugid && k.roles().contains(&e.role.as_str())));
    store.acl.push(AclEntry {
        path,
        ugid,
        role: req.role,
        propagate: false,
    });
    store.save(&state.data_dir)?;
    Ok(Json(
        json!({ "shares": shares_of(&store, k, &req.conn, &req.vm) }),
    ))
}

async fn remove(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<ShareReq>,
) -> Result<Json<Value>, ApiError> {
    let k = req.kind;
    let mut store = state.write();
    may_share(&store, &user, k, &req.conn, &req.vm)?;
    let path = k.path(&req.conn, &req.vm);
    let ugid = format!("user:{}", req.user);
    let before = store.acl.len();
    store
        .acl
        .retain(|e| !(e.path == path && e.ugid == ugid && k.roles().contains(&e.role.as_str())));
    if store.acl.len() == before {
        return Err(ApiError::not_found(format!(
            "that user has no share on this {}",
            k.word()
        )));
    }
    store.save(&state.data_dir)?;
    Ok(Json(
        json!({ "shares": shares_of(&store, k, &req.conn, &req.vm) }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::{builtin_roles, Resource, ADMIN_ROLE, DEFAULT_ROLE};

    fn user(id: &str) -> User {
        let mut u = User::new(id.into(), String::new(), id.into());
        u.id = id.into();
        u
    }

    fn store() -> Store {
        let mut s = Store {
            roles: builtin_roles(),
            users: vec![user("admin"), user("owner"), user("friend")],
            ..Default::default()
        };
        s.connections.push(crate::store::Connection {
            id: "c".into(),
            name: "hv".into(),
            url: String::new(),
            token: String::new(),
            cert: None,
            fingerprint: None,
        });
        for (id, role) in [
            ("admin", ADMIN_ROLE),
            ("owner", DEFAULT_ROLE),
            ("friend", DEFAULT_ROLE),
        ] {
            s.acl.push(AclEntry {
                path: "/".into(),
                ugid: format!("user:{id}"),
                role: role.into(),
                propagate: true,
            });
        }
        s.resources.push(Resource {
            kind: Kind::Vm,
            conn: "c".into(),
            pool: String::new(),
            name: "web".into(),
            owner: "owner".into(),
            suspended: false,
            path: String::new(),
            capacity: 0,
            blank: false,
            created_ms: 0,
        });
        s
    }

    fn share(s: &mut Store, role: &str) {
        s.acl.push(AclEntry {
            path: vm_path("c", "web"),
            ugid: "user:friend".into(),
            role: role.into(),
            propagate: false,
        });
    }

    #[test]
    fn the_owner_and_an_admin_may_share_and_nobody_else() {
        let s = store();
        assert!(may_share(&s, &user("owner"), ShareKind::Vm, "c", "web").is_ok());
        assert!(may_share(&s, &user("admin"), ShareKind::Vm, "c", "web").is_ok());
        assert!(may_share(&s, &user("friend"), ShareKind::Vm, "c", "web").is_err());
    }

    /// The default role can create VMs (VM.Allocate at /). That must not
    /// read as "may share every VM".
    #[test]
    fn creating_vms_does_not_make_you_able_to_share_others() {
        let s = store();
        let f = user("friend");
        // The role grants VM.Allocate at / so users can create; it must not
        // reach another user's VM, as delete or as share.
        assert!(s.can(&f, "/nodes/c/vms", "VM.Allocate"), "can create");
        assert!(
            !s.can(&f, &vm_path("c", "web"), "VM.Allocate"),
            "cannot delete theirs"
        );
        assert!(
            may_share(&s, &f, ShareKind::Vm, "c", "web").is_err(),
            "cannot share theirs"
        );
    }

    #[test]
    fn a_suspended_vm_cannot_be_shared_by_its_owner() {
        let mut s = store();
        s.resource_mut(Kind::Vm, "c", "web").unwrap().suspended = true;
        assert!(may_share(&s, &user("owner"), ShareKind::Vm, "c", "web").is_err());
        assert!(
            may_share(&s, &user("admin"), ShareKind::Vm, "c", "web").is_ok(),
            "an admin still can"
        );
    }

    #[test]
    fn a_share_grants_exactly_its_level_on_exactly_that_vm() {
        let mut s = store();
        share(&mut s, "VMUser");
        let f = user("friend");
        let p = vm_path("c", "web");
        assert!(s.can(&f, &p, "VM.Console") && s.can(&f, &p, "VM.PowerMgmt"));
        assert!(!s.can(&f, &p, "VM.Config"), "VMUser cannot configure");
        assert!(
            !s.can(&f, &vm_path("c", "other"), "VM.Audit"),
            "nothing else"
        );
    }

    /// VMAdmin lets them configure, but not re-share or delete: that stays
    /// with the owner.
    #[test]
    fn someone_it_was_shared_with_cannot_share_it_onward() {
        let mut s = store();
        share(&mut s, "VMAdmin");
        let f = user("friend");
        assert!(s.can(&f, &vm_path("c", "web"), "VM.Config"));
        assert!(may_share(&s, &f, ShareKind::Vm, "c", "web").is_err());
    }

    #[test]
    fn listing_shows_only_share_roles_to_users() {
        let mut s = store();
        share(&mut s, "VMUser");
        // An admin's broader grant on the same path is not a "share".
        s.acl.push(AclEntry {
            path: vm_path("c", "web"),
            ugid: "group:ops".into(),
            role: "VMAdmin".into(),
            propagate: true,
        });
        let v = shares_of(&s, ShareKind::Vm, "c", "web");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["username"], "friend");
        assert_eq!(v[0]["role"], "VMUser");
    }
}
