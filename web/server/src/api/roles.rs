use axum::extract::{Path, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{RoleDef, Scopes, SCOPES};

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/roles", get(list).post(create))
        .route("/roles/{name}", axum::routing::put(update).delete(remove))
        .layer(middleware::from_fn(auth::require_scope(
            state.clone(),
            "users",
            true,
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn role_json(r: &RoleDef) -> Value {
    json!({
        "name": r.name,
        "builtin": r.builtin,
        "scopes": r.scopes,
    })
}

/// Roles plus the scope vocabulary, so the editor renders exactly the scopes
/// the backend enforces instead of a hardcoded list that can drift.
async fn list(State(state): State<AppState>) -> Json<Value> {
    let store = state.read();
    let roles: Vec<Value> = store.roles.iter().map(role_json).collect();
    Json(json!({ "roles": roles, "scopes": SCOPES }))
}

#[derive(Deserialize)]
struct RoleReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    scopes: Scopes,
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim().to_ascii_lowercase();
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
    Ok(name)
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<RoleReq>,
) -> Result<Json<Value>, ApiError> {
    let name = valid_name(&req.name)?;
    super::users::validate_scopes(&req.scopes)?;
    let mut store = state.write();
    if store.role(&name).is_some() {
        return Err(ApiError::conflict("role already exists"));
    }
    let def = RoleDef {
        name,
        builtin: false,
        scopes: req.scopes,
    };
    store.roles.push(def.clone());
    store.save(&state.data_dir)?;
    Ok(Json(role_json(&def)))
}

/// Retune a role's permissions. Built-in roles are editable (that is the
/// "customize existing ones" half) but cannot be renamed, so users pointing at
/// them never dangle.
async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RoleReq>,
) -> Result<Json<Value>, ApiError> {
    super::users::validate_scopes(&req.scopes)?;
    let mut store = state.write();
    let Some(idx) = store.roles.iter().position(|r| r.name == name) else {
        return Err(ApiError::not_found("no such role"));
    };

    let mut def = store.roles[idx].clone();
    def.scopes = req.scopes;

    // Renaming is only allowed for custom roles, and must carry its users.
    let rename = {
        let next = req.name.trim();
        if next.is_empty() || next.eq_ignore_ascii_case(&name) {
            None
        } else if def.builtin {
            return Err(ApiError::bad_request("cannot rename a built-in role"));
        } else {
            let next = valid_name(next)?;
            if store.role(&next).is_some() {
                return Err(ApiError::conflict("role already exists"));
            }
            Some(next)
        }
    };
    if let Some(ref next) = rename {
        def.name = next.clone();
    }

    // Refuse a change that would leave nobody able to administer. Simulate it
    // against every user, since an admin may hold this role rather than the
    // built-in one.
    let mut probe = store.roles.clone();
    probe[idx] = def.clone();
    let admins = store
        .users
        .iter()
        .filter(|u| {
            let role_name = match (&rename, u.role == name) {
                (Some(next), true) => next.clone(),
                _ => u.role.clone(),
            };
            // Same precedence as Store::permission: override first, then role.
            match u.scopes.get("users") {
                Some(p) => *p == crate::store::Permission::Write,
                None => probe
                    .iter()
                    .find(|r| r.name == role_name)
                    .and_then(|r| r.scopes.get("users"))
                    .map(|p| *p == crate::store::Permission::Write)
                    .unwrap_or(false),
            }
        })
        .count();
    if admins == 0 {
        return Err(ApiError::bad_request(
            "that change would leave no administrator",
        ));
    }

    store.roles[idx] = def.clone();
    if let Some(next) = rename {
        for u in store.users.iter_mut().filter(|u| u.role == name) {
            u.role = next.clone();
        }
    }
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
    if store.roles[pos].builtin {
        return Err(ApiError::bad_request("cannot delete a built-in role"));
    }
    // Refuse while anyone still holds it, rather than silently orphaning them
    // into a role that no longer resolves (which would deny everything).
    let holders = store.users.iter().filter(|u| u.role == name).count();
    if holders > 0 {
        return Err(ApiError::bad_request(format!(
            "{holders} user(s) still use this role"
        )));
    }
    store.roles.remove(pos);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": name})))
}
