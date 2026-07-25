mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn make_codex_provider(app: &common::TestApp) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(k, v)
        .json(&json!({
            "id": "cx", "name": "Codex", "wire_format": "openai", "kind": "oauth_codex",
            "base_url": null, "api_key": null, "upstream_model": "gpt-5-codex"
        }))
        .send().await.unwrap();
}

#[tokio::test]
async fn oauth_start_returns_authorize_url() {
    let app = spawn_app().await;
    make_codex_provider(&app).await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/admin/providers/cx/oauth/start", app.base_url))
        .header(k, v)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let url = body["authorize_url"].as_str().unwrap();
    assert!(url.contains("code_challenge="));
    assert!(url.contains("state="));
}

// Regression coverage the brief left optional/deferred: exercises the full
// oauth/complete flow against a wiremock'd auth.openai.com token endpoint
// with a fake JWT id_token, per the plan's testing section for this task.
#[tokio::test]
async fn oauth_complete_exchanges_code_and_persists_account_claims() {
    let auth_server = MockServer::start().await;

    let b64 = |b: &[u8]| {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    };
    let payload = json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_test123",
            "workspace_id": "ws_test456"
        }
    });
    let fake_jwt = format!(
        "{}.{}.{}",
        b64(b"{\"alg\":\"none\"}"),
        b64(payload.to_string().as_bytes()),
        "sig"
    );

    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-123",
            "refresh_token": "rt-456",
            "id_token": fake_jwt,
            "expires_in": 3600
        })))
        .mount(&auth_server)
        .await;

    std::env::set_var(
        "CODEX_TOKEN_URL",
        format!("{}/oauth/token", auth_server.uri()),
    );

    let app = spawn_app().await;
    make_codex_provider(&app).await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let start_resp: serde_json::Value = client
        .post(format!("{}/admin/providers/cx/oauth/start", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Extract the state param from the authorize_url, exactly as a caller
    // would parse it from the real redirect URL's query string.
    let authorize_url = start_resp["authorize_url"].as_str().unwrap();
    let issued_state = authorize_url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();

    let resp = client
        .post(format!("{}/admin/providers/cx/oauth/complete", app.base_url))
        .header(&k, &v)
        .json(&json!({ "code": "auth-code-abc", "state": issued_state }))
        .send()
        .await
        .unwrap();

    std::env::remove_var("CODEX_TOKEN_URL");

    assert_eq!(resp.status(), 200);

    let os = router::providers::queries::get_oauth_state(&app.db, "cx")
        .await
        .unwrap()
        .expect("oauth state should exist after complete");
    assert_eq!(os.access_token.as_deref(), Some("at-123"));
    assert_eq!(os.refresh_token.as_deref(), Some("rt-456"));
    assert!(os.pkce_verifier.is_none(), "pkce should be cleared after complete");
    assert_eq!(
        os.provider_data["chatgpt_account_id"],
        json!("acct_test123")
    );
    assert_eq!(os.provider_data["workspace_id"], json!("ws_test456"));
}

// Regression test for the Phase 3 review: /oauth/complete stored oauth_state
// but never validated it, so it provided no real CSRF binding.
#[tokio::test]
async fn oauth_complete_rejects_state_mismatch() {
    let app = spawn_app().await;
    make_codex_provider(&app).await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers/cx/oauth/start", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/admin/providers/cx/oauth/complete", app.base_url))
        .header(&k, &v)
        .json(&json!({ "code": "auth-code-abc", "state": "wrong-state" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn provider_state_endpoint_reports_status() {
    let app = spawn_app().await;
    make_codex_provider(&app).await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/cx/state", app.base_url))
        .header(k, v)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "healthy");
    assert_eq!(body["backoff_level"], 0);
}
