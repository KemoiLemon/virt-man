use axum::extract::{Path, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::perm::{AclEntry, Quota, DEFAULT_ROLE};
use crate::state::AppState;
use crate::store::{Store, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{id}", axum::routing::put(update).delete(remove))
        .layer(middleware::from_fn(auth::require_priv(
            state.clone(),
            "/",
            "Permissions.Modify",
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

/// A user as the admin page lists them: the account, its groups, its quota,
/// and whether it administers.
fn admin_json(store: &Store, u: &User) -> Value {
    let mut v = auth::user_json(u);
    v["admin"] = json!(store.is_admin(u));
    v["quota"] = json!(u.quota);
    v["groups"] = json!(store
        .groups
        .iter()
        .filter(|g| g.members.contains(&u.id))
        .map(|g| g.name.clone())
        .collect::<Vec<_>>());
    v
}

async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = state.read();
    let users: Vec<Value> = store.users.iter().map(|u| admin_json(&store, u)).collect();
    Ok(Json(json!(users)))
}

#[derive(Deserialize)]
struct CreateUser {
    username: String,
    password: String,
    #[serde(default)]
    nickname: String,
    /// Role granted at `/`. Defaults to `User`: see its daemons and pools,
    /// create guests and networks, and so see only their own.
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    quota: Quota,
}

fn default_role() -> String {
    DEFAULT_ROLE.into()
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateUser>,
) -> Result<Json<Value>, ApiError> {
    let username = req.username.trim().to_string();
    if username.is_empty() || req.password.len() < 8 {
        return Err(ApiError::bad_request(
            "username required and password must be at least 8 characters",
        ));
    }
    let hash = auth::hash_password(&req.password)?;
    let mut store = state.write();
    if store.user_by_username(&username).is_some() {
        return Err(ApiError::conflict("username already exists"));
    }
    let role = req.role.trim();
    if !role.is_empty() && store.role(role).is_none() {
        return Err(ApiError::bad_request(format!("no such role: {role}")));
    }
    let nickname = match req.nickname.trim() {
        "" => "user".to_string(),
        n => n.to_string(),
    };
    let mut user = User::new(username, hash, nickname);
    user.quota = req.quota;
    if !role.is_empty() {
        store.acl.push(AclEntry {
            path: "/".into(),
            ugid: format!("user:{}", user.id),
            role: role.into(),
            propagate: true,
        });
    }
    for g in &req.groups {
        let Some(group) = store.groups.iter_mut().find(|x| &x.name == g) else {
            return Err(ApiError::bad_request(format!("no such group: {g}")));
        };
        group.members.push(user.id.clone());
    }
    store.users.push(user.clone());
    store.save(&state.data_dir)?;
    Ok(Json(admin_json(&store, &user)))
}

#[derive(Deserialize)]
struct UpdateUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    quota: Option<Quota>,
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateUser>,
) -> Result<Json<Value>, ApiError> {
    let hash = match &req.password {
        Some(p) if p.len() < 8 => {
            return Err(ApiError::bad_request(
                "password must be at least 8 characters",
            ))
        }
        Some(p) => Some(auth::hash_password(p)?),
        None => None,
    };
    let mut store = state.write();
    let Some(idx) = store.users.iter().position(|u| u.id == id) else {
        return Err(ApiError::not_found("no such user"));
    };
    if let Some(ref username) = req.username {
        let username = username.trim();
        if username.is_empty() {
            return Err(ApiError::bad_request("username required"));
        }
        if store
            .users
            .iter()
            .any(|u| u.id != id && u.username == username)
        {
            return Err(ApiError::conflict("username already exists"));
        }
    }
    if let Some(ref groups) = req.groups {
        if let Some(g) = groups
            .iter()
            .find(|g| !store.groups.iter().any(|x| &x.name == *g))
        {
            return Err(ApiError::bad_request(format!("no such group: {g}")));
        }
    }

    // Group membership can carry administration, so apply it to a copy and
    // refuse an outcome with no administrator left.
    let mut next_groups = store.groups.clone();
    if let Some(ref groups) = req.groups {
        for g in &mut next_groups {
            g.members.retain(|m| m != &id);
            if groups.contains(&g.name) {
                g.members.push(id.clone());
            }
        }
    }
    let was = store.admin_count();
    let prev_groups = std::mem::replace(&mut store.groups, next_groups);
    if was > 0 && store.admin_count() == 0 {
        store.groups = prev_groups;
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }

    let u = &mut store.users[idx];
    if let Some(username) = req.username {
        u.username = username.trim().to_string();
    }
    if let Some(hash) = hash {
        u.password_hash = hash;
    }
    if let Some(nickname) = req.nickname {
        u.nickname = match nickname.trim() {
            "" => "user".into(),
            n => n.into(),
        };
    }
    if let Some(quota) = req.quota {
        u.quota = quota;
    }
    let user = u.clone();
    store.save(&state.data_dir)?;
    Ok(Json(admin_json(&store, &user)))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(actor): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    if actor.id == id {
        return Err(ApiError::bad_request("cannot delete your own account"));
    }
    let mut store = state.write();
    let Some(pos) = store.users.iter().position(|u| u.id == id) else {
        return Err(ApiError::not_found("no such user"));
    };
    if store.is_admin(&store.users[pos]) && store.admin_count() <= 1 {
        return Err(ApiError::bad_request(
            "cannot delete the last administrator",
        ));
    }
    // Whatever they owned passes to the first administrator rather than
    // becoming unowned, so it stays visible and administrable.
    let heir = store
        .users
        .iter()
        .find(|u| u.id != id && store.is_admin(u))
        .map(|u| u.id.clone())
        .unwrap_or_default();
    for r in store.resources.iter_mut().filter(|r| r.owner == id) {
        r.owner = heir.clone();
    }
    let me = format!("user:{id}");
    store.acl.retain(|e| e.ugid != me);
    for g in &mut store.groups {
        g.members.retain(|m| m != &id);
    }
    store.users.remove(pos);
    store.api_keys.retain(|k| k.user_id != id);
    store.sessions.retain(|_, uid| uid != &id);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}
