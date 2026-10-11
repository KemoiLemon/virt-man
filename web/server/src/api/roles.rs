use axum::extract::{Path, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::perm::{RoleDef, PRIVILEGES};
use crate::state::AppState;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/roles", get(list).post(create))
        .route("/roles/{name}", axum::routing::put(update).delete(remove))
        .layer(middleware::from_fn(auth::require_priv(
            state.clone(),
            "/",
            "Permissions.Modify",
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn role_json(r: &RoleDef) -> Value {
    json!({ "name": r.name, "builtin": r.builtin, "privs": r.privs })
}

/// Roles plus the privilege vocabulary, so the editor renders exactly what
/// the backend enforces instead of a hardcoded list that can drift.
async fn list(State(state): State<AppState>) -> Json<Value> {
    let store = state.read();
    let roles: Vec<Value> = store.roles.iter().map(role_json).collect();
    Json(json!({ "roles": roles, "privileges": PRIVILEGES }))
}

#[derive(Deserialize)]
struct RoleReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    privs: Vec<String>,
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("role name is required"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request(
            "role name may only contain letters, digits, - and _",
        ));
    }
    Ok(name.to_string())
}

/// Reject privilege names nothing enforces, so the editor cannot save a
/// grant that silently never applies.
pub fn validate_privs(privs: &[String]) -> Result<Vec<String>, ApiError> {
    let mut out = privs.to_vec();
    if let Some(bad) = out.iter().find(|p| !PRIVILEGES.contains(&p.as_str())) {
        return Err(ApiError::bad_request(format!("unknown privilege: {bad}")));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<RoleReq>,
) -> Result<Json<Value>, ApiError> {
    let name = valid_name(&req.name)?;
    let privs = validate_privs(&req.privs)?;
    let mut store = state.write();
    if store
        .roles
        .iter()
        .any(|r| r.name.eq_ignore_ascii_case(&name))
    {
        return Err(ApiError::conflict("role already exists"));
    }
    let def = RoleDef {
        name,
        builtin: false,
        privs,
        scopes: Default::default(),
    };
    store.roles.push(def.clone());
    store.save(&state.data_dir)?;
    Ok(Json(role_json(&def)))
}

/// Retune a role while preserving whether it is built in.
async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RoleReq>,
) -> Result<Json<Value>, ApiError> {
    let privs = validate_privs(&req.privs)?;
    let mut store = state.write();
    let Some(idx) = store.roles.iter().position(|r| r.name == name) else {
        return Err(ApiError::not_found("no such role"));
    };
    let builtin = store.roles[idx].builtin;
    let next = match req.name.trim() {
        "" => name.clone(),
        n => valid_name(n)?,
    };
    if next != name
        && store
            .roles
            .iter()
            .any(|r| r.name.eq_ignore_ascii_case(&next))
    {
        return Err(ApiError::conflict("role already exists"));
    }

    // Refuse an outcome with no administrator: an admin may hold this role.
    let before = store.roles[idx].clone();
    store.roles[idx] = RoleDef {
        name: next.clone(),
        builtin,
        privs,
        scopes: Default::default(),
    };
    for e in store.acl.iter_mut().filter(|e| e.role == name) {
        e.role = next.clone();
    }
    if store.admin_count() == 0 {
        store.roles[idx] = before;
        for e in store.acl.iter_mut().filter(|e| e.role == next) {
            e.role = name.clone();
        }
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }
    let def = store.roles[idx].clone();
    store.save(&state.data_dir)?;
    Ok(Json(role_json(&def)))
}

async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(pos) = store.roles.iter().position(|r| r.name == name) else {
        return Err(ApiError::not_found("no such role"));
    };
    // Refuse while any grant uses it, rather than leaving grants that name a
    // role that no longer resolves (which would silently deny).
    let uses = store.acl.iter().filter(|e| e.role == name).count();
    if uses > 0 {
        return Err(ApiError::bad_request(format!(
            "{uses} permission(s) still use this role"
        )));
    }
    let removed = store.roles.remove(pos);
    if removed.builtin && !store.removed_builtin_roles.contains(&name) {
        store.removed_builtin_roles.push(name.clone());
    }
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": name})))
}
