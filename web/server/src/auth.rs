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
    // Persistent (Max-Age) so a browser restart does not log the user out, and
    // Secure so the token never rides a plaintext HTTP request — which now
    // matters: the same port answers plain HTTP with a redirect, and without
    // Secure a LAN client would send its session cookie in cleartext.
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=2592000")
}

pub fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
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
    let mut user = match api_key.user_id.as_deref() {
        Some(id) => store.user_by_id(id).cloned()?,
        None => {
            let mut user = User::new("ownerless-api-key".into(), String::new(), "API key".into());
            user.id = format!("key:{}", api_key.id);
            user
        }
    };
    // A key can only narrow its owner: `Store::privs` intersects with this,
    // so every check sees the cap without knowing a key was involved. An empty
    // list means "whatever the owner has".
    if api_key.user_id.is_none() || !api_key.privs.is_empty() {
        user.key_privs = Some(api_key.privs.iter().cloned().collect());
    }
    Some(user)
}

pub const LOCAL_ID: &str = "local";

/// Synthetic user for loopback mode (no auth): full admin.
pub fn local_user() -> User {
    let mut u = User::new("local".into(), String::new(), "local".into());
    u.id = LOCAL_ID.into();
    u
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
/// to hold `privilege` at `path`.
pub fn require_priv(
    state: AppState,
    path: &'static str,
    privilege: &'static str,
) -> impl Fn(Extension<User>, Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Clone
{
    move |Extension(user): Extension<User>, req: Request, next: Next| {
        let state = state.clone();
        Box::pin(async move {
            let refused = {
                let store = state.read();
                (!store.can(&user, path, privilege))
                    .then(|| crate::api::denied(&store, &user, path, privilege))
            };
            match refused {
                Some(e) => e.into_response(),
                None => next.run(req).await,
            }
        })
    }
}

/// JSON view of a user, without the password hash.
pub fn user_json(u: &User) -> serde_json::Value {
    serde_json::json!({
        "id": u.id,
        "username": u.username,
        "nickname": u.nickname,
        "avatar": u.avatar,
    })
}

/// A user as they see themselves: what they may do at `/`, so the SPA can
/// hide what the API would refuse. Cosmetic only; the API checks every call.
pub fn me_json(store: &crate::store::Store, u: &User) -> serde_json::Value {
    let mut v = user_json(u);
    v["admin"] = serde_json::json!(store.is_admin(u));
    v["privs"] = serde_json::json!(store.privs(u, "/"));
    v["quota"] = serde_json::json!(u.quota);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_cookie_persists_and_stays_https_only() {
        let c = session_cookie("tok");
        assert!(c.starts_with("virt-man-session=tok; "), "{c}");
        // A browser restart must not log the user out, and the token must never
        // ride a plaintext request (the same port answers HTTP with a redirect).
        assert!(c.contains("Max-Age=2592000"), "persistent: {c}");
        assert!(c.contains("Secure"), "https only: {c}");
        assert!(
            c.contains("HttpOnly") && c.contains("SameSite=Lax"),
            "defense in depth: {c}"
        );
    }

    #[test]
    fn clearing_the_cookie_expires_it() {
        let c = cleared_cookie();
        assert!(c.contains("Max-Age=0"), "{c}");
        assert!(
            c.contains("Secure"),
            "clear must match the set attributes: {c}"
        );
    }
}
