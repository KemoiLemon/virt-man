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
use crate::store::{random_hex, sha256_hex, ApiKey, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    // Own keys (any authenticated user), plus admin-wide listing/revocation.
    Router::new()
        .route("/me/keys", get(my_keys).post(create_key))
        .route("/me/keys/{id}", axum::routing::delete(delete_key))
        .route("/keys", get(all_keys))
        .route("/keys/{id}", axum::routing::delete(delete_any_key))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn key_json(k: &ApiKey) -> Value {
    json!({
        "id": k.id,
        "name": k.name,
        "prefix": k.prefix,
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
    name: String,
}

async fn create_key(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<Value>, ApiError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("key name is required"));
    }
    // The plaintext key is returned exactly once.
    let key = format!("vm_{}", random_hex(24));
    let mut store = state.write();
    let api_key = ApiKey {
        id: random_hex(8),
        user_id: user.id,
        name,
        hash: sha256_hex(key.as_bytes()),
        prefix: key[..11].to_string(),
    };
    store.api_keys.push(api_key.clone());
    store.save(&state.data_dir)?;
    Ok(Json(json!({
        "id": api_key.id,
        "name": api_key.name,
        "prefix": api_key.prefix,
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
