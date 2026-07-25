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
