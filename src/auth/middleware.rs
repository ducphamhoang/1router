use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::core::state::AppState;

pub fn bearer_matches(token: &str, secret: &str) -> bool {
    let token = token.as_bytes();
    let secret = secret.as_bytes();
    let max_len = token.len().max(secret.len());
    let mut diff = (token.len() ^ secret.len()) as u8;
    for i in 0..max_len {
        let a = token.get(i).copied().unwrap_or(0);
        let b = secret.get(i).copied().unwrap_or(0);
        diff |= a ^ b;
    }
    diff.ct_eq(&0).into()
}

pub fn bearer_matches_any(token: &str, secrets: &[String]) -> bool {
    secrets.iter().any(|secret| bearer_matches(token, secret))
}

fn bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "message": "unauthorized" } })),
    )
        .into_response()
}

pub async fn require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let ok = bearer_token(&req)
        .map(|token| bearer_matches_any(token, &state.config.shared_secrets))
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        unauthorized()
    }
}

pub async fn require_admin_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let required = state
        .config
        .admin_secret
        .as_deref()
        .unwrap_or(&state.config.shared_secret);
    let ok = bearer_token(&req)
        .map(|token| bearer_matches(token, required))
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        unauthorized()
    }
}

#[cfg(test)]
mod tests {
    use super::{bearer_matches, bearer_matches_any};

    #[test]
    fn bearer_match_requires_exact_secret() {
        assert!(bearer_matches("secret", "secret"));
        assert!(!bearer_matches("secret", "secrex"));
        assert!(!bearer_matches("secret", "secret-longer"));
    }

    #[test]
    fn bearer_match_accepts_rotated_secret_set() {
        let secrets = vec!["current".to_string(), "previous".to_string()];
        assert!(bearer_matches_any("previous", &secrets));
        assert!(bearer_matches_any("current", &secrets));
        assert!(!bearer_matches_any("unknown", &secrets));
    }
}
