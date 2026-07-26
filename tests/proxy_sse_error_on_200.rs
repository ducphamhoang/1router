mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

// Accepted known limitation: an upstream can commit to HTTP 200 and then emit an
// error event mid-SSE-stream (e.g. a usage_limit_reached event after some partial
// content). Pure passthrough has already flushed the 200 status line by the time
// the error event arrives, so it cannot convert it into a different HTTP status —
// it can only relay the body as-is. This test asserts the router does exactly
// that: it does not crash or choke on the embedded error, and both the partial
// content and the error text make it through to the client untouched.
#[tokio::test]
async fn error_event_inside_http_200_sse_is_passed_through() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
               data: {\"error\":{\"message\":\"usage_limit_reached\"}}\n\n";
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

    // The HTTP status is 200 (committed) and the error is inside the body — this is the
    // documented accepted limitation; the router must relay it, not choke.
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("usage_limit_reached"));
    assert!(text.contains("partial"));
}
