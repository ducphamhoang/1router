//! Regression tests for the 2026-09-23 security audit's response-hardening
//! findings: SEC-07 (upstream header relay), SEC-08 (security headers),
//! SEC-10 (redirects).
mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(app: &common::TestApp, wire: &str, base_url: &str) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    for (path, body) in [
        (
            "/admin/providers",
            json!({
                "id": "p1", "name": "p1", "wire_format": wire, "kind": "passthrough",
                "base_url": base_url, "api_key": "sk-test", "upstream_model": "m"
            }),
        ),
        ("/admin/pools", json!({ "id": "pool", "wire_format": wire })),
    ] {
        let resp = client
            .post(format!("{}{path}", app.base_url))
            .header(&k, &v)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{path}: {}", resp.status());
    }
    client
        .put(format!("{}/admin/pools/pool/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "p1", "priority": 1 }))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn only_allowlisted_upstream_headers_reach_the_caller() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true}))
                .insert_header("set-cookie", "__cf_bm=abc; Path=/")
                .insert_header("openai-organization", "org-secret")
                .insert_header("x-request-id", "req_123")
                .insert_header("x-ratelimit-remaining-requests", "99"),
        )
        .mount(&upstream)
        .await;
    let app = spawn_app().await;
    setup(&app, "openai", &upstream.uri()).await;

    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    assert!(h.get("set-cookie").is_none());
    assert!(h.get("openai-organization").is_none());
    assert_eq!(h.get("x-request-id").unwrap(), "req_123");
    assert_eq!(h.get("x-ratelimit-remaining-requests").unwrap(), "99");
}

#[tokio::test]
async fn upstream_redirects_are_not_followed() {
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"stolen": true})))
        .mount(&elsewhere)
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/steal", elsewhere.uri())),
        )
        .mount(&upstream)
        .await;
    let app = spawn_app().await;
    // Anthropic wire: the key travels as x-api-key, which reqwest would
    // have kept on a cross-host redirect.
    setup(&app, "anthropic", &upstream.uri()).await;

    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "pool", "max_tokens": 1, "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 200, "the redirect target's body must not be relayed");
    assert!(
        elsewhere.received_requests().await.unwrap().is_empty(),
        "the provider key must never be sent to the redirect target"
    );
}

#[tokio::test]
async fn security_headers_on_every_response_and_no_store_on_admin() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();

    let health = client.get(format!("{}/health", app.base_url)).send().await.unwrap();
    let h = health.headers();
    assert!(h["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "no-referrer");

    let (k, v) = auth_header(&app.secret);
    let admin = client
        .get(format!("{}/admin/users", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(admin.headers()["cache-control"], "no-store");

    // Even an auth failure (rejected before any handler) is covered.
    let denied = client.get(format!("{}/admin/users", app.base_url)).send().await.unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(denied.headers()["x-frame-options"], "DENY");
}
