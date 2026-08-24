mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method};
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

async fn add_pool_member_with_model(
    app: &common::TestApp,
    provider_id: &str,
    priority: i64,
    model_override: &str,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .put(format!("{}/admin/pools/gpt-4o/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": provider_id, "priority": priority, "model_override": model_override }))
        .send().await.unwrap()
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

/// Proves the whole point of `migrations/0005_pool_member_model_identity.sql`:
/// one provider can occupy two slots in the same pool with different
/// `model_override`s, both PUTs succeed (no unique-violation error from the
/// old `(pool_id, provider_id)` PK), and both are listed as members.
#[tokio::test]
async fn same_provider_two_models_can_both_join_one_pool() {
    let app = spawn_app().await;
    create_pool(&app).await;
    add_provider(&app, "shared", "http://127.0.0.1:1/unused").await;

    let r1 = add_pool_member_with_model(&app, "shared", 1, "model-a").await;
    assert_eq!(r1.status(), 200, "first (provider, model) member must succeed");
    let r2 = add_pool_member_with_model(&app, "shared", 2, "model-b").await;
    assert_eq!(
        r2.status(),
        200,
        "second member with the SAME provider but a DIFFERENT model must succeed \
         (this is exactly what the old (pool_id, provider_id) PK forbade)"
    );

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let members: serde_json::Value = client
        .get(format!("{}/admin/pools/gpt-4o/members", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(members.as_array().unwrap().len(), 2);
}

/// One provider, two pool members differing only by `model_override`,
/// `strategy: round_robin` - proves consecutive requests alternate
/// between the two models using a single provider row, the exact
/// real-world scenario (one Command Code OAuth account serving several
/// models) that motivated this fix.
#[tokio::test]
async fn round_robin_alternates_across_two_models_of_one_provider() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "model-a" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "model-a"})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "model-b" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "model-b"})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    create_pool_with_strategy(&app, "round_robin", None).await;
    add_provider(&app, "shared", &format!("{}/v1/chat/completions", upstream.uri())).await;
    add_pool_member_with_model(&app, "shared", 1, "model-a").await;
    add_pool_member_with_model(&app, "shared", 2, "model-b").await;

    let resp1: serde_json::Value = chat_request(&app).await.json().await.unwrap();
    let resp2: serde_json::Value = chat_request(&app).await.json().await.unwrap();
    let resp3: serde_json::Value = chat_request(&app).await.json().await.unwrap();

    assert_eq!(resp1["served_by"], "model-a");
    assert_eq!(resp2["served_by"], "model-b");
    assert_eq!(resp3["served_by"], "model-a");
}

/// The critical regression this plan's runtime-keying fix (Task 3) exists
/// for: one provider, two models. The first model 500s. Without the fix,
/// the failover loop would skip the second (same-provider) member because
/// the cooldown was recorded against the bare provider id - the request
/// would 503 instead of failing over. With the fix, the second model is
/// tried within the SAME request and serves successfully.
#[tokio::test]
async fn same_provider_different_model_members_fail_over_to_each_other() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "flaky-model" })))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "healthy-model" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "healthy-model"})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    create_pool(&app).await;
    add_provider(&app, "shared", &format!("{}/v1/chat/completions", upstream.uri())).await;
    add_pool_member_with_model(&app, "shared", 1, "flaky-model").await;
    add_pool_member_with_model(&app, "shared", 2, "healthy-model").await;

    let resp = chat_request(&app).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["served_by"], "healthy-model");
}

/// The other half of the runtime-keying fix: a `NonRetryable` error on one
/// model must not misconfigure its siblings. First request hits the
/// broken model and gets a 400 (misconfiguring it); a SEPARATE, later
/// request for the healthy model must still succeed - proving the first
/// model's `Misconfigured` flag didn't take down the whole provider.
#[tokio::test]
async fn nonretryable_error_on_one_model_does_not_misconfigure_its_sibling() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "broken-model" })))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "bad model"})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "model": "healthy-model" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"served_by": "healthy-model"})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    // Two separate single-member pools (not one round-robin/priority pool)
    // so each request deterministically targets one specific model,
    // isolating "did the OTHER model get poisoned" from any
    // rotation/failover-ordering behavior.
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    for pool_id in ["broken-pool", "healthy-pool"] {
        client
            .post(format!("{}/admin/pools", app.base_url))
            .header(&k, &v)
            .json(&json!({ "id": pool_id, "wire_format": "openai" }))
            .send()
            .await
            .unwrap();
    }
    add_provider(&app, "shared", &format!("{}/v1/chat/completions", upstream.uri())).await;
    for (pool_id, model) in [("broken-pool", "broken-model"), ("healthy-pool", "healthy-model")] {
        client
            .put(format!("{}/admin/pools/{pool_id}/members", app.base_url))
            .header(&k, &v)
            .json(&json!({ "provider_id": "shared", "priority": 1, "model_override": model }))
            .send()
            .await
            .unwrap();
    }

    let broken_resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({ "model": "broken-pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(broken_resp.status(), 400, "broken model's own error passes through");

    let healthy_resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({ "model": "healthy-pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        healthy_resp.status(),
        200,
        "the healthy model must still serve - it must not have been misconfigured \
         by the broken model's failure, since they share a provider id but not a runtime_key"
    );
}
