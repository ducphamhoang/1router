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
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn validate_model_reports_ok_on_a_successful_upstream_reply() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(&app, "p1", &format!("{}/v1/chat/completions", upstream.uri())).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/admin/providers/p1/validate-model", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-5.6-sol" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn validate_model_reports_failure_on_a_non_success_upstream_reply() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404).set_body_string("model not found"))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(&app, "p1", &format!("{}/v1/chat/completions", upstream.uri())).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/admin/providers/p1/validate-model", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "not-a-real-model" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 404);
    assert!(body["message"].as_str().unwrap().contains("model not found"));
}

#[tokio::test]
async fn validate_model_falls_back_to_the_providers_own_upstream_model_when_blank() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    add_provider(&app, "p1", &format!("{}/v1/chat/completions", upstream.uri())).await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/admin/providers/p1/validate-model", app.base_url))
        .header(k, v)
        .json(&json!({ "model": null }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
}
