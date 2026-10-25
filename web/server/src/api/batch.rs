//! `POST /batch`: several GETs in one round trip.
//!
//! The 1 s poll used to issue ten-odd reads per server per second. Each of
//! those is dispatched here through the app's own router with the caller's
//! credentials, so authentication, the ACL, the proxy cache and every route's
//! own checks apply exactly as they would to the individual request -- there is
//! no second permission path to keep in step.

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, Method};
use axum::middleware;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::auth;
use crate::state::AppState;

/// The assembled app, set once at startup (the router cannot hold itself).
static INNER: OnceLock<Router> = OnceLock::new();

pub fn set_inner(app: Router) {
    let _ = INNER.set(app);
}

const MAX_PATHS: usize = 64;
const MAX_BODY: usize = 32 * 1024 * 1024;

#[derive(Deserialize)]
struct Req {
    paths: Vec<String>,
}

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/batch", post(batch))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

async fn batch(headers: HeaderMap, Json(req): Json<Req>) -> Json<Value> {
    let Some(app) = INNER.get() else {
        return Json(json!([]));
    };
    if req.paths.len() > MAX_PATHS {
        return Json(Value::Array(vec![
            json!({"status": 400, "body": {"error": "too many paths in one batch"}});
            req.paths.len().min(MAX_PATHS)
        ]));
    }
    let one = |path: String| {
        let app = app.clone();
        let headers = headers.clone();
        async move {
            // Reads only, and never the batch itself.
            if !path.starts_with('/') || path.starts_with("/batch") {
                return json!({"status": 400, "body": {"error": "not batchable"}});
            }
            let Ok(mut r) = Request::builder()
                .method(Method::GET)
                .uri(&path)
                .body(Body::empty())
            else {
                return json!({"status": 400, "body": {"error": "bad path"}});
            };
            // Only what identifies the caller; the batch's own content headers
            // would describe the wrong body.
            for name in [header::COOKIE, header::AUTHORIZATION, "x-api-key".parse().unwrap()] {
                if let Some(v) = headers.get(&name) {
                    r.headers_mut().insert(name, v.clone());
                }
            }
            if let Some(v) = headers.get("x-virt-man-cache") {
                r.headers_mut().insert("x-virt-man-cache", v.clone());
            }
            let Ok(res) = app.oneshot(r).await; // Infallible
            let status = res.status().as_u16();
            let bytes = axum::body::to_bytes(res.into_body(), MAX_BODY)
                .await
                .unwrap_or_default();
            let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
            json!({"status": status, "body": body})
        }
    };
    Json(Value::Array(join_all(req.paths.into_iter().map(one)).await))
}
