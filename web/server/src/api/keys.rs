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
use crate::store::{random_hex, random_name, sha256_hex, ApiKey, Scopes, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    // Own keys (any authenticated user), plus admin-wide listing/creation and
    // revocation.
    Router::new()
        .route("/me/keys", get(my_keys).post(create_key))
        .route("/me/keys/{id}", axum::routing::delete(delete_key))
        .route("/keys", get(all_keys).post(create_any_key))
        .route("/keys/{id}", axum::routing::delete(delete_any_key))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn key_json(k: &ApiKey) -> Value {
    json!({
        "id": k.id,
        "name": k.name,
        "prefix": k.prefix,
        "user_id": k.user_id,
        "scopes": k.scopes,
    })
}

async fn my_keys(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Json<Value> {
    let keys: Vec<Value> = state
        .read()
        .api_keys
        .iter()
        .filter(|k| k.user_id == user.id)
        .map(key_json)
        .collect();
    Json(json!(keys))
}

async fn all_keys(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "users", false)?;
    let keys: Vec<Value> = state.read().api_keys.iter().map(key_json).collect();
    Ok(Json(json!(keys)))
}

#[derive(Deserialize)]
struct CreateKeyReq {
    /// Omitted or blank means "generate one", so a key can be minted without
    /// inventing a label.
    #[serde(default)]
    name: String,
    /// Per-scope ceiling. Omitted or empty means "inherit the owner".
    #[serde(default)]
    scopes: Scopes,
    /// Admin-only: whose key this is. Defaults to the caller.
    #[serde(default)]
    user_id: Option<String>,
}

/// Mint a key owned by the caller.
async fn create_key(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<Value>, ApiError> {
    issue_key(&state, &user.id, req)
}

/// Admin: mint a key for any user, with its own scope ceiling.
async fn create_any_key(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "users", true)?;
    let owner = req.user_id.clone().unwrap_or_else(|| user.id.clone());
    issue_key(&state, &owner, req)
}

fn issue_key(state: &AppState, owner_id: &str, req: CreateKeyReq) -> Result<Json<Value>, ApiError> {
    let name = match req.name.trim() {
        "" => random_name(),
        n => n.to_string(),
    };
    super::users::validate_scopes(&req.scopes)?;

    let mut store = state.write();
    if store.user_by_id(owner_id).is_none() {
        return Err(ApiError::bad_request("no such user"));
    }
    // The plaintext key is returned exactly once.
    let key = format!("vm_{}", random_hex(24));
    let api_key = ApiKey {
        id: random_hex(8),
        user_id: owner_id.to_string(),
        name,
        hash: sha256_hex(key.as_bytes()),
        prefix: key[..11].to_string(),
        scopes: req.scopes,
    };
    store.api_keys.push(api_key.clone());
    store.save(&state.data_dir)?;
    Ok(Json(json!({
        "id": api_key.id,
        "name": api_key.name,
        "prefix": api_key.prefix,
        "user_id": api_key.user_id,
        "scopes": api_key.scopes,
        "key": key,
    })))
}

async fn delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(pos) = store
        .api_keys
        .iter()
        .position(|k| k.id == id && k.user_id == user.id)
    else {
        return Err(ApiError::not_found("no such API key"));
    };
    store.api_keys.remove(pos);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}

async fn delete_any_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "users", true)?;
    let mut store = state.write();
    let Some(pos) = store.api_keys.iter().position(|k| k.id == id) else {
        return Err(ApiError::not_found("no such API key"));
    };
    store.api_keys.remove(pos);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}
