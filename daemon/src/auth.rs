use axum::extract::Extension;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::path::Path;
use subtle::ConstantTimeEq;

/// The configured auth token. Cheap to clone, compared once per request.
#[derive(Clone)]
pub struct Auth {
    token: String,
}

impl Auth {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let token = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read token file {}: {e}", path.display()))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(format!("token file {} is empty", path.display()));
        }
        Ok(Self { token })
    }

    pub fn token_ok(&self, candidate: &str) -> bool {
        // Constant-time compare; lengths are public info, only content is secret.
        self.token.len() == candidate.len()
            && bool::from(self.token.as_bytes().ct_eq(candidate.as_bytes()))
    }

    fn header_ok(&self, request: &Request) -> bool {
        request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| self.token_ok(t))
            .unwrap_or(false)
    }
}

/// Require `Authorization: Bearer <token>` on every request.
pub async fn require_token(
    Extension(auth): Extension<Auth>,
    request: Request,
    next: Next,
) -> Response {
    if auth.header_ok(&request) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

pub fn check_query_token(query: &str, auth: &Auth) -> bool {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == "token")
        .map(|(_, v)| auth.token_ok(v))
        .unwrap_or(false)
}
