//! Users: named bearer credentials for `/v1/*`.
//!
//! The admin issues one key per user; `auth::middleware::require_bearer`
//! resolves the presented key to a [`Caller`] and stashes it in the request
//! extensions so the proxy can attribute the request in `request_log` and
//! the dataset log. Only a SHA-256 of the raw key is persisted - the raw key
//! is returned exactly once, on create/rotate. Upstream provider credentials
//! are unaffected. Design:
//! `docs/superpowers/specs/2026-08-28-user-credentials-design.md`.

pub mod queries;
pub mod routes;

use axum::http::HeaderMap;
use axum::Router;

use crate::core::state::AppState;

/// `user_id` recorded for requests authenticated with the shared secret.
/// Reserved: no user may take this id.
pub const ADMIN_USER_ID: &str = "admin";

/// Who made a `/v1/*` request. Inserted into the request extensions by
/// `auth::middleware::require_bearer`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    /// A `users.id`, `"admin"` for the shared secret, `None` for anonymous
    /// open-access calls.
    pub user_id: Option<String>,
}

impl Caller {
    pub fn admin() -> Self {
        Caller {
            user_id: Some(ADMIN_USER_ID.to_string()),
        }
    }

    pub fn anonymous() -> Self {
        Caller::default()
    }

    /// Authenticated with the shared secret.
    pub fn is_admin(&self) -> bool {
        self.user_id.as_deref() == Some(ADMIN_USER_ID)
    }
}

/// The credential a client presented: `Authorization: Bearer <key>` (OpenAI
/// SDKs) or `x-api-key: <key>` (Anthropic SDKs).
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

pub fn routes() -> Router<AppState> {
    routes::routes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presented_key_reads_bearer_then_x_api_key() {
        let mut h = HeaderMap::new();
        assert_eq!(presented_key(&h), None);
        h.insert("x-api-key", "k2".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k2"));
        h.insert("authorization", "Bearer k1".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k1"));
    }
}
