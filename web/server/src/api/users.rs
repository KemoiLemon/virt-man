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
use crate::store::{random_hex, Scopes, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{id}", axum::routing::put(update).delete(remove))
        .layer(middleware::from_fn(auth::require_scope(
            state.clone(),
            "users",
            true,
        )))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let users: Vec<Value> = state.read().users.iter().map(auth::user_json).collect();
    Ok(Json(json!(users)))
}

#[derive(Deserialize)]
struct CreateUser {
    username: String,
    password: String,
    #[serde(default)]
    nickname: String,
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    scopes: Scopes,
}

fn default_role() -> String {
    "viewer".into()
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
    let mut store = state.write();
    if store.user_by_username(&username).is_some() {
        return Err(ApiError::conflict("username already exists"));
    }
    if store.role(&req.role).is_none() {
        return Err(ApiError::bad_request(format!("no such role: {}", req.role)));
    }
    validate_scopes(&req.scopes)?;
    let user = User {
        id: random_hex(8),
        username,
        password_hash: auth::hash_password(&req.password)?,
        role: req.role,
        nickname: if req.nickname.trim().is_empty() {
            "user".to_string()
        } else {
            req.nickname.trim().to_string()
        },
        avatar: None,
        scopes: req.scopes,
    };
    store.users.push(user.clone());
    store.save(&state.data_dir)?;
    Ok(Json(auth::user_json(&user)))
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
    role: Option<String>,
    #[serde(default)]
    scopes: Option<Scopes>,
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(actor): axum::extract::Extension<User>,
    Json(req): Json<UpdateUser>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(idx) = store.users.iter().position(|u| u.id == id) else {
        return Err(ApiError::not_found("no such user"));
    };

    // Validate against other users before taking the mutable borrow.
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
    if let Some(ref role) = req.role {
        if store.role(role).is_none() {
            return Err(ApiError::bad_request(format!("no such role: {role}")));
        }
    }
    if let Some(ref scopes) = req.scopes {
        validate_scopes(scopes)?;
    }

    // Build the updated user first, then refuse the change if it would leave
    // the install with no administrator. Admin-ness is derived from the
    // effective users:write permission, so a role change, a scope override or
    // a retuned role can all cause it — checking the outcome catches every
    // path instead of only self-demotion by role.
    let mut updated = store.users[idx].clone();
    if let Some(username) = req.username {
        updated.username = username.trim().to_string();
    }
    if let Some(password) = req.password {
        if password.len() < 8 {
            return Err(ApiError::bad_request(
                "password must be at least 8 characters",
            ));
        }
        updated.password_hash = auth::hash_password(&password)?;
    }
    if let Some(nickname) = req.nickname {
        updated.nickname = if nickname.trim().is_empty() {
            "user".into()
        } else {
            nickname.trim().into()
        };
    }
    if let Some(role) = req.role {
        updated.role = role;
    }
    if let Some(scopes) = req.scopes {
        updated.scopes = scopes;
    }

    let was_admin = store.is_admin(&store.users[idx]);
    let is_admin = store.is_admin(&updated);
    if was_admin && !is_admin && store.admin_count() <= 1 {
        return Err(ApiError::bad_request(
            "cannot remove the last administrator",
        ));
    }
    let _ = actor;

    store.users[idx] = updated;
    let user = store.users[idx].clone();
    store.save(&state.data_dir)?;
    Ok(Json(auth::user_json(&user)))
}

/// Reject scope names nothing enforces. Without this the UI could save an
/// override that silently never applies (as "domains" did).
pub fn validate_scopes(scopes: &Scopes) -> Result<(), ApiError> {
    for name in scopes.keys() {
        if !crate::store::SCOPES.contains(&name.as_str()) {
            return Err(ApiError::bad_request(format!(
                "unknown scope: {name} (valid: {})",
                crate::store::SCOPES.join(", ")
            )));
        }
    }
    Ok(())
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
    store.users.remove(pos);
    store.api_keys.retain(|k| k.user_id != id);
    store.sessions.retain(|_, uid| uid != &id);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}
