mod common;
use common::{auth_header, spawn_app};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(upstream: &MockServer) -> (common::TestApp, reqwest::Client) {
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
        .json(&json!({ "id": "p1", "name": "p1", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "m" }))
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
    (app, client)
}

#[tokio::test]
async fn responses_non_streaming_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "m", "usage": {"prompt_tokens": 2, "completion_tokens": 3},
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "pong"}}]
        })))
        .mount(&upstream)
        .await;
    let (app, client) = setup(&upstream).await;
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/responses", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "input": "ping", "instructions": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "pong");
    assert_eq!(body["usage"]["total_tokens"], 5);

    let sent: Value = serde_json::from_slice(&upstream.received_requests().await.unwrap().last().unwrap().body).unwrap();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][1]["content"], "ping");
}

#[tokio::test]
async fn responses_streaming_translates_chat_sse() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"po\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"ng\"},\"finish_reason\":\"stop\"}]}\n\n\
               data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .mount(&upstream)
        .await;
    let (app, client) = setup(&upstream).await;
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/responses", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o", "input": "ping", "stream": true }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("event: response.created"));
    assert!(text.contains("event: response.output_text.delta"));
    assert!(text.contains("event: response.completed"));
    assert!(text.contains("\"text\":\"pong\""));
}

#[tokio::test]
async fn responses_bad_requests_are_rejected() {
    let upstream = MockServer::start().await;
    let (app, client) = setup(&upstream).await;
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .post(format!("{}/v1/responses", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "gpt-4o" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
