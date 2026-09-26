use axum::extract::{Extension, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::future::Future;
use std::pin::Pin;

use crate::state::AppState;
use crate::store::{sha256_hex, User};

pub const SESSION_COOKIE: &str = "virt-man-session";

pub fn hash_password(password: &str) -> Result<String, String> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| format!("hash password: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

pub fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax")
}

pub fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Resolve the logged-in user from the session cookie, if any.
fn session_user(state: &AppState, req: &Request) -> Option<User> {
    let token = req
        .headers()
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .filter_map(|c| c.split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)?
        .1
        .to_string();
    let hash = sha256_hex(token.as_bytes());
    let store = state.read();
    let user_id = store.sessions.get(&hash)?;
    store.user_by_id(user_id).cloned()
}

/// Resolve the user from an API key (Authorization: Bearer or X-API-Key).
fn api_key_user(state: &AppState, req: &Request) -> Option<User> {
    let key = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| req.headers().get("x-api-key").and_then(|v| v.to_str().ok()))?;
    let hash = sha256_hex(key.as_bytes());
    let store = state.read();
    let api_key = store.api_keys.iter().find(|k| k.hash == hash)?;
    let mut user = store.user_by_id(&api_key.user_id).cloned()?;
    // A key can only narrow its owner. Collapse the intersection into the
    // user's own overrides so every downstream permission() call sees the
    // capped values without knowing a key was involved.
    user.scopes = store.key_effective_scopes(&user, api_key);
    Some(user)
}

/// Synthetic user for loopback mode (no auth): full admin.
pub fn local_user() -> User {
    User {
        id: "local".into(),
        username: "local".into(),
        password_hash: String::new(),
        role: "admin".into(),
        nickname: "local".into(),
        avatar: None,
        scopes: Default::default(),
    }
}

/// Middleware factory: require authentication (session cookie or API key) and
/// attach the user as an `Extension<User>`.
pub fn require_user(
    state: AppState,
) -> impl Fn(Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Clone {
    move |mut req: Request, next: Next| {
        let state = state.clone();
        Box::pin(async move {
            if state.auth_disabled {
                req.extensions_mut().insert(local_user());
                return next.run(req).await;
            }
            let user = session_user(&state, &req).or_else(|| api_key_user(&state, &req));
            match user {
                Some(user) => {
                    req.extensions_mut().insert(user);
                    next.run(req).await
                }
                None => StatusCode::UNAUTHORIZED.into_response(),
            }
        })
    }
}

/// Middleware factory: require the authenticated user (set by `require_user`)
/// to hold `scope` permission at the given level (read or write).
pub fn require_scope(
    state: AppState,
    scope: &'static str,
    write: bool,
) -> impl Fn(Extension<User>, Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Clone
{
    move |Extension(user): Extension<User>, req: Request, next: Next| {
        let state = state.clone();
        Box::pin(async move {
            let allowed = state.read().can(&user, scope, write);
            if allowed {
                next.run(req).await
            } else {
                StatusCode::FORBIDDEN.into_response()
            }
        })
    }
}

/// JSON view of a user, without the password hash.
pub fn user_json(u: &User) -> serde_json::Value {
    serde_json::json!({
        "id": u.id,
        "username": u.username,
        "role": u.role,
        "nickname": u.nickname,
        "avatar": u.avatar,
        "scopes": u.scopes,
    })
}
