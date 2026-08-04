mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn commandcode_models_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn add_provider(app: &common::TestApp, id: &str, wire_format: &str, base_url: &str) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": id, "name": id, "wire_format": wire_format, "kind": "passthrough",
            "base_url": base_url, "api_key": "sk-test", "upstream_model": "real-model"
        }))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn list_models_returns_the_providers_live_model_list() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "id": "gpt-5.6-sol" }, { "id": "gpt-5.6-luna" }]
        })))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(
        &app,
        "p1",
        "openai",
        &format!("{}/v1/chat/completions", upstream.uri()),
    )
    .await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/p1/list-models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["models"], json!(["gpt-5.6-sol", "gpt-5.6-luna"]));
}

#[tokio::test]
async fn list_models_uses_anthropic_auth_headers_for_an_anthropic_provider() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "claude-sonnet-5" }]
        })))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(
        &app,
        "p1",
        "anthropic",
        &format!("{}/v1/messages", upstream.uri()),
    )
    .await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/p1/list-models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["models"], json!(["claude-sonnet-5"]));
}

#[tokio::test]
async fn list_models_reports_failure_when_upstream_returns_an_error() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(
        &app,
        "p1",
        "openai",
        &format!("{}/v1/chat/completions", upstream.uri()),
    )
    .await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/p1/list-models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["reason"].as_str().unwrap().contains("invalid api key"));
}

#[tokio::test]
async fn list_models_reports_unsupported_for_oauth_codex_providers() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "cx", "name": "cx", "wire_format": "openai", "kind": "oauth_codex",
            "upstream_model": "pending"
        }))
        .send()
        .await
        .unwrap();

    let resp = client
        .get(format!("{}/admin/providers/cx/list-models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["reason"].as_str().unwrap().contains("no discoverable"));
}

#[tokio::test]
async fn commandcode_list_models_uses_unauthenticated_fixed_endpoint() {
    let _guard = commandcode_models_env_lock().lock().unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "id": "cc-1", "name": "CC One", "context_length": 200000 }]
        })))
        .mount(&upstream)
        .await;
    std::env::set_var(
        "ROUTER_COMMANDCODE_MODELS_URL",
        format!("{}/provider/v1/models", upstream.uri()),
    );

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "cc", "name": "cc", "wire_format": "openai", "kind": "oauth_command_code",
            "upstream_model": "cc-1"
        }))
        .send()
        .await
        .unwrap();
    let resp = client
        .get(format!("{}/admin/providers/cc/list-models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["models"], json!(["cc-1"]));
    std::env::remove_var("ROUTER_COMMANDCODE_MODELS_URL");
}
