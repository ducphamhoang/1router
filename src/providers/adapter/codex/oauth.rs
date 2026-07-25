use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::core::error::RefreshError;

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn generate_pkce() -> Pkce {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let verifier = b64url(&raw);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn build_authorize_url(state: &str, challenge: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
}

fn token_url() -> String {
    // Test hook: allow overriding the token endpoint for wiremock (mirrors
    // refresh.rs's CODEX_TOKEN_URL, since exchange and refresh hit the same
    // endpoint with different content-types).
    std::env::var("CODEX_TOKEN_URL").unwrap_or_else(|_| TOKEN_URL.to_string())
}

pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<TokenSet, RefreshError> {
    // Code exchange uses form-urlencoded (differs from refresh which is JSON).
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CODEX_CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let resp = http
        .post(token_url())
        .form(&form)
        .send()
        .await
        .map_err(|e| RefreshError::Transient(format!("token request failed: {e}")))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        if body.contains("invalid_grant") {
            return Err(RefreshError::InvalidGrant);
        }
        return Err(RefreshError::Transient(format!("token exchange {body}")));
    }

    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| RefreshError::Transient(format!("token parse: {e}")))?;

    Ok(TokenSet {
        access_token: j["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: j["refresh_token"].as_str().map(|s| s.to_string()),
        id_token: j["id_token"].as_str().map(|s| s.to_string()),
        expires_in: j["expires_in"].as_i64(),
    })
}

pub struct AccountClaims {
    pub chatgpt_account_id: Option<String>,
    pub workspace_id: Option<String>,
}

pub fn decode_account_claims(id_token: &str) -> AccountClaims {
    let empty = AccountClaims {
        chatgpt_account_id: None,
        workspace_id: None,
    };
    let payload_b64 = match id_token.split('.').nth(1) {
        Some(p) => p,
        None => return empty,
    };
    let bytes = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) {
        Ok(b) => b,
        Err(_) => return empty,
    };
    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return empty,
    };
    let auth = &json["https://api.openai.com/auth"];
    AccountClaims {
        chatgpt_account_id: auth["chatgpt_account_id"].as_str().map(|s| s.to_string()),
        workspace_id: auth["workspace_id"].as_str().map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = generate_pkce();
        assert!(p.verifier.len() >= 43);
        // recompute S256(verifier) and compare
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(p.verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(p.challenge, expected);
    }

    #[test]
    fn authorize_url_contains_required_params() {
        let url = build_authorize_url("state123", "challenge456");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge=challenge456"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state123"));
        assert!(
            url.contains(&urlencoding::encode("http://localhost:1455/auth/callback").into_owned())
        );
    }

    #[test]
    fn decode_account_claims_reads_openai_auth_claim() {
        // build a fake unsigned JWT: header.payload.sig (base64url), payload holds the claim
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123",
                "workspace_id": "ws_456"
            }
        });
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let jwt = format!(
            "{}.{}.{}",
            b64(b"{\"alg\":\"none\"}"),
            b64(payload.to_string().as_bytes()),
            "sig"
        );
        let claims = decode_account_claims(&jwt);
        assert_eq!(claims.chatgpt_account_id.as_deref(), Some("acct_123"));
        assert_eq!(claims.workspace_id.as_deref(), Some("ws_456"));
    }
}
