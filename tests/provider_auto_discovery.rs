mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Creating a passthrough provider triggers a best-effort background fetch
/// of its `/models`, which should show up in `GET /v1/models` as
/// `<provider_id>/<model>` entries without any separate list-models call.
/// The fetch is async, so poll briefly rather than asserting immediately.
#[tokio::test]
async fn creating_a_provider_eventually_populates_v1_models_with_its_discovered_models() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "deepseek-v4-flash" }, { "id": "deepseek-v4-pro" }]
        })))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "deepseek_api", "name": "deepseek_api", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk-test", "upstream_model": "deepseek-v4-flash"
        }))
        .send()
        .await
        .unwrap();

    let mut found = false;
    for _ in 0..50 {
        let resp = client
            .get(format!("{}/v1/models", app.base_url))
            .header(&k, &v)
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let ids: Vec<String> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();
        if ids.contains(&"deepseek_api/deepseek-v4-pro".to_string()) {
            found = true;
            assert!(ids.contains(&"deepseek_api/deepseek-v4-flash".to_string()));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(found, "expected /v1/models to eventually list deepseek_api/deepseek-v4-pro");
}

/// A provider whose live `/models` call fails (dead upstream) must not
/// break /v1/models for everyone else - it's simply absent.
#[tokio::test]
async fn a_provider_with_a_failing_models_endpoint_is_silently_absent_from_v1_models() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "dead_provider", "name": "dead_provider", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1/v1/chat/completions",
            "api_key": "sk-test", "upstream_model": "m"
        }))
        .send()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let resp = client
        .get(format!("{}/v1/models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
}
