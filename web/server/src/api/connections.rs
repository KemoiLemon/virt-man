use axum::extract::{Path, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::daemon;
use crate::state::AppState;
use crate::store::{random_hex, Connection, User};

pub fn routes(state: &AppState) -> Router<AppState> {
    let read = Router::new()
        .route("/connections", get(list))
        .route("/connections/{id}/test", get(test))
        .route("/connections/{id}/cert", get(fetch_cert))
        .layer(middleware::from_fn(auth::require_scope(
            state.clone(),
            "connections",
            false,
        )));
    let write = Router::new()
        .route("/connections", axum::routing::post(create))
        .route(
            "/connections/{id}",
            axum::routing::put(update).delete(remove),
        )
        .layer(middleware::from_fn(auth::require_scope(
            state.clone(),
            "connections",
            true,
        )));
    Router::new()
        .merge(read)
        .merge(write)
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn conn_json(c: &Connection) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "url": c.url,
        "fingerprint": c.fingerprint,
        "has_token": !c.token.is_empty(),
        "has_cert": c.cert.is_some(),
    })
}

async fn list(State(state): State<AppState>) -> Json<Value> {
    let conns: Vec<Value> = state.read().connections.iter().map(conn_json).collect();
    Json(json!(conns))
}

#[derive(Deserialize)]
struct UpsertReq {
    name: String,
    url: String,
    #[serde(default)]
    token: String,
    /// PEM of the pinned cert, or "auto" to fetch it from the daemon.
    #[serde(default)]
    cert: Option<String>,
    #[serde(default)]
    generate_token: bool,
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<UpsertReq>,
) -> Result<Json<Value>, ApiError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let url = req.url.trim().to_string();
    daemon::parse_url(&url)?;

    let token = if req.generate_token {
        format!("vm_{}", random_hex(24))
    } else {
        req.token.trim().to_string()
    };
    if token.is_empty() {
        return Err(ApiError::bad_request(
            "supply an existing token or enable generate_token",
        ));
    }

    let (cert, fingerprint) = match req.cert.as_deref() {
        Some("auto") | None if url.starts_with("https://") => {
            let (pem, fp) = daemon::fetch_cert(&url)
                .await
                .map_err(|e| ApiError::bad_request(format!("certificate fetch failed: {e}")))?;
            (Some(pem), Some(fp))
        }
        Some(pem) if !pem.is_empty() => {
            let fp = daemon::fingerprint_of_pem(pem)
                .map_err(|e| ApiError::bad_request(format!("invalid certificate: {e}")))?;
            (Some(pem.to_string()), Some(fp))
        }
        _ => (None, None),
    };

    let mut store = state.write();
    let conn = Connection {
        id: random_hex(8),
        name,
        url,
        token,
        cert,
        fingerprint,
    };
    store.connections.push(conn.clone());
    store.save(&state.data_dir)?;
    Ok(Json(conn_json(&conn)))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpsertReq>,
) -> Result<Json<Value>, ApiError> {
    let url = req.url.trim().to_string();
    daemon::parse_url(&url)?;

    let mut store = state.write();
    let Some(conn) = store.connections.iter_mut().find(|c| c.id == id) else {
        return Err(ApiError::not_found("no such connection"));
    };
    conn.name = if req.name.trim().is_empty() {
        conn.name.clone()
    } else {
        req.name.trim().into()
    };
    conn.url = url;
    if req.generate_token {
        conn.token = format!("vm_{}", random_hex(24));
    } else if !req.token.trim().is_empty() {
        conn.token = req.token.trim().to_string();
    }
    // Re-pin cert if explicitly provided, otherwise keep existing pin for a
    // URL change the user has already confirmed.
    if let Some(pem) = req.cert.as_deref().filter(|p| !p.is_empty()) {
        conn.cert = Some(pem.to_string());
        conn.fingerprint = daemon::fingerprint_of_pem(pem).ok();
    }
    let conn = conn.clone();
    store.save(&state.data_dir)?;
    Ok(Json(conn_json(&conn)))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut store = state.write();
    let Some(pos) = store.connections.iter().position(|c| c.id == id) else {
        return Err(ApiError::not_found("no such connection"));
    };
    store.connections.remove(pos);
    store.save(&state.data_dir)?;
    Ok(Json(json!({"deleted": id})))
}

/// Fetch and return the daemon's certificate + fingerprint so the client can
/// confirm before pinning. Does not modify the stored connection.
async fn fetch_cert(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    super::enforce(&state, &user, "connections", false)?;
    let url = {
        let store = state.read();
        let Some(c) = store.connection(&id) else {
            return Err(ApiError::not_found("no such connection"));
        };
        c.url.clone()
    };
    let (pem, fp) = daemon::fetch_cert(&url)
        .await
        .map_err(|e| ApiError::bad_request(format!("certificate fetch failed: {e}")))?;
    Ok(Json(json!({"cert": pem, "fingerprint": fp})))
}

/// Test a connection by hitting the daemon's /health through the pinned client.
async fn test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conn = {
        let store = state.read();
        store.connection(&id).cloned()
    }
    .ok_or_else(|| ApiError::not_found("no such connection"))?;

    let client = daemon::http_client(&conn)
        .map_err(|e| ApiError::bad_request(format!("connection error: {e}")))?;
    let resp = client
        .get(format!("{}/health", conn.url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", conn.token))
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("daemon unreachable: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Ok(Json(json!({
        "ok": status.is_success(),
        "status": status.as_u16(),
        "body": body,
    })))
}
