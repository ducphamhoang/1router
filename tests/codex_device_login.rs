//! Headless (device-code) Codex login. Its own test binary: the auth
//! endpoints are only overridable through the process-global
//! `CODEX_DEVICE_AUTH_URL` / `CODEX_TOKEN_URL` env vars, and the two tests
//! here share them, so they run one at a time behind `ENV`.

mod common;
use common::{auth_header, spawn_app};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV: Mutex<()> = Mutex::const_new(());

fn fake_id_token(account: &str) -> String {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": account } });
    format!("{}.{}.sig", b64(b"{\"alg\":\"none\"}"), b64(payload.to_string().as_bytes()))
}

async fn setup(auth: &MockServer) -> (common::TestApp, reqwest::Client, (String, String)) {
    std::env::set_var("CODEX_DEVICE_AUTH_URL", auth.uri());
    std::env::set_var("CODEX_TOKEN_URL", format!("{}/oauth/token", auth.uri()));
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let (k, v) = (k.to_string(), v.to_string());
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "cx", "name": "Codex", "wire_format": "openai", "kind": "oauth_codex",
            "base_url": null, "api_key": null, "upstream_model": "gpt-5.5"
        }))
        .send()
        .await
        .unwrap();
    (app, client, (k, v))
}

async fn wait_for_terminal(app: &common::TestApp, client: &reqwest::Client, h: &(String, String)) -> Value {
    for _ in 0..50 {
        let body: Value = client
            .get(format!("{}/admin/providers/cx/oauth/device/status", app.base_url))
            .header(&h.0, &h.1)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if body["status"] != "pending" {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("device login never finished");
}

#[tokio::test]
async fn device_login_polls_until_approved_then_stores_tokens() {
    let _g = ENV.lock().await;
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/usercode"))
        .and(body_partial_json(json!({ "client_id": "app_EMoamEEZ73f0CkXaXp7hrann" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "dev-1", "user_code": "ABCD-1234", "interval": "1"
        })))
        .expect(1)
        .mount(&auth)
        .await;
    // Pending once, then approved.
    Mock::given(method("POST"))
        .and(path("/deviceauth/token"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&auth)
        .await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/token"))
        .and(body_partial_json(json!({ "device_auth_id": "dev-1", "user_code": "ABCD-1234" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "auth-code-1", "code_verifier": "server-verifier", "code_challenge": "x"
        })))
        .with_priority(2)
        .mount(&auth)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("code_verifier=server-verifier"))
        .and(body_string_contains("deviceauth%2Fcallback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-dev", "refresh_token": "rt-dev",
            "id_token": fake_id_token("acct-dev"), "expires_in": 3600
        })))
        .expect(1)
        .mount(&auth)
        .await;

    let (app, client, h) = setup(&auth).await;
    let start: Value = client
        .post(format!("{}/admin/providers/cx/oauth/device/start", app.base_url))
        .header(&h.0, &h.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(start["user_code"], "ABCD-1234");
    assert_eq!(start["verification_url"], "https://auth.openai.com/codex/device");

    let done = wait_for_terminal(&app, &client, &h).await;
    std::env::remove_var("CODEX_DEVICE_AUTH_URL");
    std::env::remove_var("CODEX_TOKEN_URL");
    assert_eq!(done["status"], "success", "{done}");

    let os = router::providers::queries::get_oauth_state(&app.db, "cx").await.unwrap().unwrap();
    assert_eq!(os.access_token.as_deref(), Some("at-dev"));
    assert_eq!(os.refresh_token.as_deref(), Some("rt-dev"));
    assert_eq!(os.provider_data["chatgpt_account_id"], "acct-dev");
}

#[tokio::test]
async fn device_login_surfaces_an_upstream_rejection() {
    let _g = ENV.lock().await;
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "dev-2", "usercode": "WXYZ", "interval": 1
        })))
        .mount(&auth)
        .await;
    Mock::given(method("POST"))
        .and(path("/deviceauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\":\"expired_token\"}"))
        .mount(&auth)
        .await;

    let (app, client, h) = setup(&auth).await;
    let resp = client
        .post(format!("{}/admin/providers/cx/oauth/device/start", app.base_url))
        .header(&h.0, &h.1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let done = wait_for_terminal(&app, &client, &h).await;
    std::env::remove_var("CODEX_DEVICE_AUTH_URL");
    std::env::remove_var("CODEX_TOKEN_URL");
    assert_eq!(done["status"], "error");
    assert!(done["error"].as_str().unwrap().contains("expired_token"), "{done}");
    // Terminal status is reported once, then cleared.
    let again: Value = client
        .get(format!("{}/admin/providers/cx/oauth/device/status", app.base_url))
        .header(&h.0, &h.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["status"], "not_started");
}

#[tokio::test]
async fn device_login_rejects_non_codex_providers() {
    let _g = ENV.lock().await;
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "pt", "name": "pt", "wire_format": "openai", "kind": "passthrough",
            "base_url": "https://api.example.com", "api_key": "sk", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{}/admin/providers/pt/oauth/device/start", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
