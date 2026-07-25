use chrono::{DateTime, Duration, Utc};

use crate::core::error::RefreshError;
use crate::providers::adapter::codex::oauth::{TokenSet, CODEX_CLIENT_ID, TOKEN_URL};
use crate::providers::adapter::Credentials;

// `access_expires_at` tracks the short-lived ACCESS token (~1h for Codex,
// per `expires_in` on both exchange and refresh), not the ~8-day refresh
// token. A multi-day lead window here would make needs_refresh always true
// (an hour-scale expiry is always "within 5 days"), so the background task
// would re-refresh every tick regardless of actual need - fixed per the
// Phase 3 review to a lead window scaled to the access token's own
// lifetime instead.
const REFRESH_LEAD: Duration = Duration::minutes(5);

fn token_url() -> String {
    // Test hook: allow overriding the token endpoint for wiremock.
    std::env::var("CODEX_TOKEN_URL").unwrap_or_else(|_| TOKEN_URL.to_string())
}

pub fn needs_refresh(creds: &Credentials, now: DateTime<Utc>) -> bool {
    match creds.access_expires_at {
        Some(exp) => exp - now <= REFRESH_LEAD,
        None => true, // unknown expiry -> refresh proactively
    }
}

pub async fn refresh_tokens(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<TokenSet, RefreshError> {
    // Refresh uses a JSON body (differs from form-encoded code exchange).
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CODEX_CLIENT_ID,
        "scope": "openid profile email offline_access"
    });
    let resp = http
        .post(token_url())
        .json(&body)
        .send()
        .await
        .map_err(|e| RefreshError::Transient(format!("refresh request failed: {e}")))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        if text.contains("invalid_grant") {
            return Err(RefreshError::InvalidGrant);
        }
        return Err(RefreshError::Transient(format!("refresh failed: {text}")));
    }

    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| RefreshError::Transient(format!("refresh parse: {e}")))?;
    Ok(TokenSet {
        access_token: j["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: j["refresh_token"].as_str().map(|s| s.to_string()),
        id_token: j["id_token"].as_str().map(|s| s.to_string()),
        expires_in: j["expires_in"].as_i64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::Credentials;
    use chrono::{Duration, Utc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn creds_expiring_in_minutes(minutes: i64) -> Credentials {
        Credentials {
            access_expires_at: Some(Utc::now() + Duration::minutes(minutes)),
            refresh_token: Some("rt".into()),
            ..Default::default()
        }
    }

    #[test]
    fn needs_refresh_true_when_within_5_minutes() {
        assert!(needs_refresh(&creds_expiring_in_minutes(3), Utc::now()));
        assert!(!needs_refresh(&creds_expiring_in_minutes(30), Utc::now()));
    }

    #[test]
    fn needs_refresh_true_when_already_expired() {
        assert!(needs_refresh(&creds_expiring_in_minutes(-10), Utc::now()));
    }

    #[test]
    fn needs_refresh_true_when_no_expiry_known() {
        let c = Credentials { access_expires_at: None, ..Default::default() };
        assert!(needs_refresh(&c, Utc::now()));
    }

    #[tokio::test]
    async fn refresh_invalid_grant_maps_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_grant\"}"))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        // point refresh at the mock by overriding the URL via the env hook (see impl note)
        std::env::set_var("CODEX_TOKEN_URL", format!("{}/oauth/token", server.uri()));
        let res = refresh_tokens(&http, "rt").await;
        std::env::remove_var("CODEX_TOKEN_URL");
        assert!(matches!(res, Err(crate::core::error::RefreshError::InvalidGrant)));
    }
}
