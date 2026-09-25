use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

use super::ApiError;

pub fn routes() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
}

async fn health() -> Result<Json<Value>, ApiError> {
    crate::libvirt::with_conn(|c| Ok(c.get_lib_version()?))
        .await
        .map(|_| Json(json!({"status": "ok"})))
        .map_err(ApiError::internal)
}

async fn version() -> Result<Json<Value>, ApiError> {
    let lib = crate::libvirt::with_conn(|c| Ok(c.get_lib_version()?))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "daemon": env!("CARGO_PKG_VERSION"),
        "libvirt": format_libvirt(lib),
    })))
}

fn format_libvirt(v: u32) -> String {
    format!("{}.{}.{}", v / 1_000_000, (v / 1_000) % 1_000, v % 1_000)
}
