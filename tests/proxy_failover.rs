mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn add_provider(app: &common::TestApp, id: &str, base_url: &str) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": id, "name": id, "wire_format": "openai", "kind": "passthrough",
            "base_url": base_url, "api_key": "sk-test", "upstream_model": "real-model"
        }))
        .send().await.unwrap();
}

async fn add_pool_member(app: &common::TestApp, provider_id: &str, priority: i64) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .put(format!("{}/admin/pools/gpt-4o/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": provider_id, "priority": priority }))
        .send().await.unwrap();
}

async fn create_pool(app: &common::TestApp) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
        .send().await.unwrap();
}

async fn create_pool_with_strategy(app: &common::TestApp, strategy: &str, sticky_limit: Option<i64>) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let mut body = json!({ "id": "gpt-4o", "wire_format": "openai", "strategy": strategy });
    if let Some(n) = sticky_limit {
        body["sticky_limit"] = json!(n);
    }
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&body)
        .send().await.unwrap();
}

async fn chat_request(app: &common::TestApp) -> reqwest::Response {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send().await.unwrap()
}

#[tokio::test]
async fn fails_over_from_500_to_200() {
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad)
        .await;
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&good)
        .await;

    let app = spawn_app().await;
    create_pool(&app).await;
    add_provider(&app, "bad", &format!("{}/v1/chat/completions", bad.uri())).await;
    add_provider(&app, "good", &format!("{}/v1/chat/completions", good.uri())).await;
    add_pool_member(&app, "bad", 1).await;
    add_pool_member(&app, "good", 2).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send().await.unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn all_unavailable_is_503_with_tried_header() {
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&bad)
        .await;

    let app = spawn_app().await;
    create_pool(&app).await;
    add_provider(&app, "bad", &format!("{}/v1/chat/completions", bad.uri())).await;
    add_pool_member(&app, "bad", 1).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send().await.unwrap();

    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("x-1router-tried"));
}

// Regression test for the Phase 2 review finding: NonRetryable passthrough was
// forcing content-type: text/plain on relayed upstream error bodies, which can
// make SDK clients misparse a JSON error as plain text.
#[tokio::test]
async fn non_retryable_error_preserves_upstream_content_type() {
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "error": { "message": "bad request" } })),
        )
        .mount(&bad)
        .await;

    let app = spawn_app().await;
    create_pool(&app).await;
    add_provider(&app, "bad", &format!("{}/v1/chat/completions", bad.uri())).await;
    add_pool_member(&app, "bad", 1).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "got content-type: {content_type}"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], "bad request");
}

/// End-to-end (real bound TCP listener + real HTTP round-trips, per the
/// implementation plan's Task 7) proof that a `round_robin` pool actually
/// alternates which upstream serves consecutive requests - not just a
/// unit-level check of `select()`'s ordering in isolation.
#[tokio::test]
async fn round_robin_alternates_across_two_healthy_providers() {
    let server_a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "a"})))
        .mount(&server_a)
        .await;
    let server_b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "b"})))
        .mount(&server_b)
        .await;

    let app = spawn_app().await;
    create_pool_with_strategy(&app, "round_robin", None).await;
    add_provider(&app, "a", &format!("{}/v1/chat/completions", server_a.uri())).await;
    add_provider(&app, "b", &format!("{}/v1/chat/completions", server_b.uri())).await;
    add_pool_member(&app, "a", 1).await;
    add_pool_member(&app, "b", 2).await;

    // sticky_limit defaults to 1, so priority order ("a" first) holds for
    // the first request, then rotates on the second.
    let resp1: serde_json::Value = chat_request(&app).await.json().await.unwrap();
    let resp2: serde_json::Value = chat_request(&app).await.json().await.unwrap();

    assert_eq!(resp1["served_by"], "a");
    assert_eq!(resp2["served_by"], "b");
}

#[tokio::test]
async fn round_robin_respects_sticky_limit_across_requests() {
    let server_a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "a"})))
        .mount(&server_a)
        .await;
    let server_b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "b"})))
        .mount(&server_b)
        .await;

    let app = spawn_app().await;
    create_pool_with_strategy(&app, "round_robin", Some(2)).await;
    add_provider(&app, "a", &format!("{}/v1/chat/completions", server_a.uri())).await;
    add_provider(&app, "b", &format!("{}/v1/chat/completions", server_b.uri())).await;
    add_pool_member(&app, "a", 1).await;
    add_pool_member(&app, "b", 2).await;

    let resp1: serde_json::Value = chat_request(&app).await.json().await.unwrap();
    let resp2: serde_json::Value = chat_request(&app).await.json().await.unwrap();
    let resp3: serde_json::Value = chat_request(&app).await.json().await.unwrap();

    assert_eq!(resp1["served_by"], "a", "sticky_limit 2: first request hits the head");
    assert_eq!(resp2["served_by"], "a", "sticky_limit 2: second request stays on the same head");
    assert_eq!(resp3["served_by"], "b", "third request rotates to the next member");
}

/// Proves the design claim from Task 3: rotation only changes which member
/// is tried *first* - the rest of the rotated list still serves as the
/// failover tail, so round-robin and failover are the same ordered list,
/// not two competing mechanisms.
#[tokio::test]
async fn round_robin_still_fails_over_within_one_request() {
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad)
        .await;
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "good"})))
        .mount(&good)
        .await;

    let app = spawn_app().await;
    create_pool_with_strategy(&app, "round_robin", None).await;
    add_provider(&app, "bad", &format!("{}/v1/chat/completions", bad.uri())).await;
    add_provider(&app, "good", &format!("{}/v1/chat/completions", good.uri())).await;
    add_pool_member(&app, "bad", 1).await;
    add_pool_member(&app, "good", 2).await;

    let resp = chat_request(&app).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["served_by"], "good");
}
