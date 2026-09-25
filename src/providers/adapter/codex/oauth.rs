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

/// Device-code ("headless") login, as `codex login --device-auth` does it:
/// the user enters a short code at [`DEVICE_VERIFY_URL`] on any device while
/// the server polls; no localhost redirect is involved. Not a public API -
/// endpoints mirror the Codex CLI (checked against codex-cli 0.150.1).
pub const DEVICE_AUTH_BASE: &str = "https://auth.openai.com/api/accounts";
pub const DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

fn device_auth_base() -> String {
    // Test hook, same as CODEX_TOKEN_URL.
    std::env::var("CODEX_DEVICE_AUTH_URL").unwrap_or_else(|_| DEVICE_AUTH_BASE.to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    /// Poll interval the server asked for (1..=60 s).
    pub interval: std::time::Duration,
}

/// Parse `/deviceauth/usercode`. The code field has shipped as both
/// `user_code` and `usercode`, and `interval` as a string or a number.
pub fn parse_device_code(j: &serde_json::Value) -> Option<DeviceCode> {
    let device_auth_id = j["device_auth_id"].as_str()?.to_string();
    let user_code = j["user_code"].as_str().or_else(|| j["usercode"].as_str())?.to_string();
    let secs = match &j["interval"] {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .unwrap_or(5)
    .clamp(1, 60);
    Some(DeviceCode {
        device_auth_id,
        user_code,
        interval: std::time::Duration::from_secs(secs),
    })
}

pub async fn request_device_code(http: &reqwest::Client) -> Result<DeviceCode, String> {
    let resp = http
        .post(format!("{}/deviceauth/usercode", device_auth_base()))
        .json(&serde_json::json!({ "client_id": CODEX_CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::core::http_client::read_text_truncated(resp, crate::core::http_client::MAX_ERROR_BODY).await;
        return Err(if status == reqwest::StatusCode::NOT_FOUND {
            "device code login is not available (endpoint returned 404)".to_string()
        } else {
            format!("device code request returned {status}: {body}")
        });
    }
    let j: serde_json::Value = resp.json().await.map_err(|e| format!("device code parse: {e}"))?;
    parse_device_code(&j).ok_or_else(|| "device code response is missing device_auth_id/user_code".to_string())
}

/// What `/deviceauth/token` hands back once the user approved: an ordinary
/// authorization code plus the PKCE verifier the server generated for it.
pub struct DeviceGrant {
    pub authorization_code: String,
    pub code_verifier: String,
}

/// One poll. `Ok(None)` = not approved yet (the endpoint answers 403/404
/// while pending).
pub async fn poll_device_token(http: &reqwest::Client, code: &DeviceCode) -> Result<Option<DeviceGrant>, String> {
    let resp = http
        .post(format!("{}/deviceauth/token", device_auth_base()))
        .json(&serde_json::json!({
            "device_auth_id": code.device_auth_id,
            "user_code": code.user_code,
        }))
        .send()
        .await
        .map_err(|e| format!("device token poll failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let body = crate::core::http_client::read_text_truncated(resp, crate::core::http_client::MAX_ERROR_BODY).await;
        return Err(format!("device token poll returned {status}: {body}"));
    }
    let j: serde_json::Value = resp.json().await.map_err(|e| format!("device token parse: {e}"))?;
    match (j["authorization_code"].as_str(), j["code_verifier"].as_str()) {
        (Some(c), Some(v)) => Ok(Some(DeviceGrant {
            authorization_code: c.to_string(),
            code_verifier: v.to_string(),
        })),
        _ => Err("device token response is missing authorization_code/code_verifier".to_string()),
    }
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
    exchange_code_with_redirect(http, code, verifier, REDIRECT_URI).await
}

/// `redirect_uri` must match the flow the code came from: [`REDIRECT_URI`]
/// for the browser flow, [`DEVICE_REDIRECT_URI`] for device-code login.
pub async fn exchange_code_with_redirect(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet, RefreshError> {
    // Code exchange uses form-urlencoded (differs from refresh which is JSON).
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
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
        let body = crate::core::http_client::read_text_truncated(resp, crate::core::http_client::MAX_ERROR_BODY).await;
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
    fn device_code_accepts_both_field_spellings_and_interval_shapes() {
        let a = parse_device_code(&serde_json::json!({
            "device_auth_id": "d1", "user_code": "ABCD-1234", "interval": "5"
        }))
        .unwrap();
        assert_eq!(a.user_code, "ABCD-1234");
        assert_eq!(a.interval, std::time::Duration::from_secs(5));
        let b = parse_device_code(&serde_json::json!({
            "device_auth_id": "d1", "usercode": "WXYZ", "interval": 0
        }))
        .unwrap();
        assert_eq!(b.user_code, "WXYZ");
        assert_eq!(b.interval, std::time::Duration::from_secs(1), "clamped up");
        let c = parse_device_code(&serde_json::json!({ "device_auth_id": "d1", "user_code": "Q" })).unwrap();
        assert_eq!(c.interval, std::time::Duration::from_secs(5), "default");
        assert!(parse_device_code(&serde_json::json!({ "user_code": "Q" })).is_none());
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
