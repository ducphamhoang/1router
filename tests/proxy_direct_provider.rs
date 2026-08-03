mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn add_provider(app: &common::TestApp, id: &str, wire_format: &str, base_url: &str) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": id, "name": id, "wire_format": wire_format, "kind": "passthrough",
            "base_url": base_url, "api_key": "sk-test", "upstream_model": "default-model"
        }))
        .send()
        .await
        .unwrap();
}

/// `<provider_id>/<model>` must work with zero pools created - that's the
/// point (no throwaway 1-member pool per model a provider offers).
#[tokio::test]
async fn calling_provider_slash_model_routes_directly_with_no_pool_at_all() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(&app, "deepseek_api", "openai", &format!("{}/v1/chat/completions", upstream.uri())).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "deepseek_api/deepseek-v4-pro", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // Provider creation also fires a background, best-effort GET .../models
    // against this same mock server (the auto-discovery cache warm-up) -
    // filter to the actual chat-completions POST rather than assume it's
    // the only request received.
    let received = upstream.received_requests().await.unwrap();
    let posts: Vec<_> = received
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let sent_body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(sent_body["model"], "deepseek-v4-pro", "must forward the requested model, not the provider's default upstream_model");
}

#[tokio::test]
async fn direct_provider_addressing_returns_400_for_an_unknown_provider() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "nope/some-model", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn direct_provider_addressing_respects_wire_format() {
    let app = spawn_app().await;
    add_provider(&app, "claude_api", "anthropic", "https://unused.example.com").await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    // hitting the OpenAI-shaped endpoint against an Anthropic provider must
    // not match - same rule a real pool enforces.
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "claude_api/claude-sonnet-5", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

/// A real pool still takes priority even when a provider of the same name
/// exists - direct addressing is purely a fallback for the `/`-containing
/// case, which a bare pool id can never be anyway.
#[tokio::test]
async fn a_real_pool_is_tried_first_and_wins_over_direct_addressing() {
    let pool_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"via": "pool"})))
        .mount(&pool_upstream)
        .await;

    let app = spawn_app().await;
    add_provider(&app, "p1", "openai", &format!("{}/v1/chat/completions", pool_upstream.uri())).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "shared-name", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/shared-name/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "p1", "priority": 1 }))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "shared-name", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["via"], "pool");
}
