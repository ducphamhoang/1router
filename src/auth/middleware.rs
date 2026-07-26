use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::core::state::AppState;

pub fn bearer_matches(token: &str, secret: &str) -> bool {
    token.len() == secret.len() && token.as_bytes().ct_eq(secret.as_bytes()).into()
}

pub async fn require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| bearer_matches(token, &state.config.shared_secret))
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "unauthorized" } })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::bearer_matches;

    #[test]
    fn bearer_match_requires_exact_secret() {
        assert!(bearer_matches("secret", "secret"));
        assert!(!bearer_matches("secret", "secrex"));
        assert!(!bearer_matches("secret", "secret-longer"));
    }
}
