mod common;

use common::{auth_header, spawn_app, TestApp};
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_commandcode_env(upstream: &MockServer) {
    std::env::set_var("ROUTER_COMMANDCODE_BASE_URL", upstream.uri());
    std::env::set_var(
        "ROUTER_COMMANDCODE_MODELS_URL",
        format!("{}/provider/v1/models", upstream.uri()),
    );
}

async fn create_cc(app: &TestApp, pool_id: &str, wire: &str) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":wire,"kind":"oauth_command_code","upstream_model":"cc-model"})).send().await.unwrap();
    client
        .post(format!(
            "{}/admin/providers/cc/commandcode/key",
            app.base_url
        ))
        .header(&k, &v)
        .json(&json!({"api_key":"cc-key"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({"id":pool_id,"wire_format":wire}))
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/{pool_id}/members", app.base_url))
        .header(k, v)
        .json(&json!({"provider_id":"cc","priority":1}))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn openai_wire_streaming_end_to_end() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let upstream = MockServer::start().await;
    set_commandcode_env(&upstream);
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"cc-model"}]})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST")).and(path("/alpha/generate")).and(header("authorization", "Bearer cc-key")).and(header("x-command-code-version", "0.29.0")).and(header("x-cli-environment", "production")).and(header("x-taste-learning", "true")).and(header("x-co-flag", "false")).respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/x-ndjson").set_body_string("{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n")).mount(&upstream).await;
    let app = spawn_app().await;
    create_cc(&app, "cc-pool", "openai").await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let response = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({"model":"cc-pool","messages":[{"role":"user","content":"hi"}],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("data: "));
    assert!(body.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn anthropic_wire_streaming_end_to_end() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let upstream = MockServer::start().await;
    set_commandcode_env(&upstream);
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"cc-model"}]})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST")).and(path("/alpha/generate")).respond_with(ResponseTemplate::new(200).set_body_string("{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n")).mount(&upstream).await;
    let app = spawn_app().await;
    create_cc(&app, "cc-claude", "anthropic").await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let response = client.post(format!("{}/v1/messages", app.base_url)).header(k, v).json(&json!({"model":"cc-claude","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"stream":true})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: message_start"));
    assert!(body.contains("event: message_stop"));
}

#[tokio::test]
async fn non_streaming_aggregates() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let upstream = MockServer::start().await;
    set_commandcode_env(&upstream);
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"cc-model"}]})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST")).and(path("/alpha/generate")).respond_with(ResponseTemplate::new(200).set_body_string("{\"type\":\"text-delta\",\"text\":\"hello\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n")).mount(&upstream).await;
    let app = spawn_app().await;
    create_cc(&app, "cc-json", "openai").await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let response = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({"model":"cc-json","messages":[],"stream":false}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
}

#[tokio::test]
async fn upstream_429_cools_the_provider_and_fails_over() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let upstream = MockServer::start().await;
    let fallback = MockServer::start().await;
    set_commandcode_env(&upstream);
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"cc-model"}]})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/alpha/generate"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .mount(&fallback)
        .await;
    let app = spawn_app().await;
    create_cc(&app, "failover", "openai").await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"fallback","name":"fallback","wire_format":"openai","kind":"passthrough","base_url":format!("{}/v1/chat/completions",fallback.uri()),"api_key":"k","upstream_model":"m"})).send().await.unwrap();
    client
        .put(format!("{}/admin/pools/failover/members", app.base_url))
        .header(&k, &v)
        .json(&json!({"provider_id":"fallback","priority":2}))
        .send()
        .await
        .unwrap();
    let response = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({"model":"failover","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["ok"],
        true
    );
}

#[tokio::test]
async fn a_401_marks_the_provider_misconfigured_rather_than_attempting_a_refresh() {
    let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
    let upstream = MockServer::start().await;
    set_commandcode_env(&upstream);
    Mock::given(method("GET"))
        .and(path("/provider/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"cc-model"}]})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/alpha/generate"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&upstream)
        .await;
    let app = spawn_app().await;
    create_cc(&app, "auth-fail", "openai").await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let _ = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({"model":"auth-fail","messages":[]}))
        .send()
        .await
        .unwrap();
    let state = client
        .get(format!("{}/admin/providers/cc/state", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(state["status"], "misconfigured");
}
