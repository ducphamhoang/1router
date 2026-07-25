mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn models_lists_pool_ids() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();

    let resp = client
        .get(format!("{}/v1/models", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "gpt-4o");
}

#[tokio::test]
async fn streaming_passthrough_preserves_sse_body() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"delta\":\"hi\"}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(
            &json!({ "id": "p1", "name": "p1", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "m" }),
        )
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/gpt-4o/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "p1", "priority": 1 }))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "messages": [], "stream": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"));
}
