mod common;
use common::{auth_header, spawn_app};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Reads every JSONL record written under `app.dataset_log_dir/{provider_id}`,
/// across all date files (there's only ever one in these tests, but this
/// doesn't hardcode the date format). Empty if the provider directory
/// doesn't exist at all (nothing was ever written for it).
async fn read_records(app: &common::TestApp, provider_id: &str) -> Vec<serde_json::Value> {
    let dir = app.dataset_log_dir.join(provider_id);
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    while let Some(entry) = rd.next_entry().await.unwrap() {
        let content = tokio::fs::read_to_string(entry.path()).await.unwrap();
        for line in content.lines() {
            out.push(serde_json::from_str(line).unwrap());
        }
    }
    out
}

async fn wait_for_writer() {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

async fn patch_provider(app: &common::TestApp, id: &str, body: serde_json::Value) {
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .patch(format!("{}/admin/providers/{id}", app.base_url))
        .header(&k, &v)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "PATCH /admin/providers/{id} failed");
}

#[tokio::test]
async fn dataset_logging_writes_a_jsonl_record_for_an_enabled_provider_streaming_response() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
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
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "p1", "name": "p1", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "m" }))
        .send()
        .await
        .unwrap();
    patch_provider(&app, "p1", json!({ "dataset_logging": true })).await;
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
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
        .json(&json!({ "model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}], "stream": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"));

    wait_for_writer().await;
    let records = read_records(&app, "p1").await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["pool_id"], "gpt-4o");
    assert_eq!(r["provider_id"], "p1");
    assert_eq!(r["model"], "m");
    assert_eq!(r["stream"], true);
    assert_eq!(r["complete"], true);
    assert_eq!(r["wire_format"], "openai");
    assert!(r["input_body"].as_str().unwrap().contains("\"hi\""));
    assert!(r["output_body"].as_str().unwrap().contains("[DONE]"));
    assert!(r["latency_ms"]["total_ms"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn dataset_logging_writes_a_record_for_a_non_streaming_response_too() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "p1", "name": "p1", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "m" }))
        .send()
        .await
        .unwrap();
    patch_provider(&app, "p1", json!({ "dataset_logging": true })).await;
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
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
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    wait_for_writer().await;
    let records = read_records(&app, "p1").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["stream"], false);
    assert_eq!(records[0]["complete"], true);
    assert!(records[0]["output_body"].as_str().unwrap().contains("\"ok\":true"));
}

#[tokio::test]
async fn dataset_logging_uses_the_provider_default_for_direct_addressing() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "deepseek_api", "name": "deepseek_api", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "default-model" }))
        .send()
        .await
        .unwrap();
    patch_provider(&app, "deepseek_api", json!({ "dataset_logging": true })).await;

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({ "model": "deepseek_api/deepseek-v4-pro", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    wait_for_writer().await;
    let records = read_records(&app, "deepseek_api").await;
    assert_eq!(records.len(), 1);
    assert!(records[0]["pool_id"].is_null(), "direct addressing has no pool");
    assert_eq!(records[0]["model"], "deepseek-v4-pro");

    // Flip it off: no new record should appear for a second call.
    patch_provider(&app, "deepseek_api", json!({ "dataset_logging": false })).await;
    let resp2 = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "deepseek_api/deepseek-v4-pro", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    wait_for_writer().await;
    let records2 = read_records(&app, "deepseek_api").await;
    assert_eq!(records2.len(), 1, "still just the one record from before the toggle was flipped off");
}

#[tokio::test]
async fn dataset_logging_is_off_by_default_and_writes_nothing() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
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
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
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
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    wait_for_writer().await;
    assert!(read_records(&app, "p1").await.is_empty());
}

/// The regression test for the toggle-gate-placement bug the first draft
/// of this plan had: the per-attempt gate must be resolved from the
/// *winning* attempt's own (provider, member_override), not from
/// `selection.providers[0]` before the failover loop starts. Member A
/// (priority 1, override false, unreachable base_url) fails over to
/// member B (priority 2, override true) - only B's attempt should be
/// logged.
///
/// `PutMember`/`ProviderPatch` don't yet expose `dataset_logging_override`
/// on the wire (that's Task 6) - set it via a direct DB write instead,
/// then force a snapshot reload via an unrelated no-op provider PATCH
/// (every provider/pool mutation endpoint calls `reload_snapshot`, so this
/// picks up the raw DB change too).
#[tokio::test]
async fn dataset_logging_fires_for_the_winning_failover_attempt_not_the_first_one_tried() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    // Member A: unreachable upstream, so it always fails over. Provider-level
    // dataset_logging is false (irrelevant here - it never wins a request).
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "a", "name": "a", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1/v1/chat/completions", "api_key": "sk", "upstream_model": "m" }))
        .send()
        .await
        .unwrap();
    // Member B: the real upstream, provider-level dataset_logging false too
    // - only the member override (set below) should turn logging on.
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "b", "name": "b", "wire_format": "openai", "kind": "passthrough",
            "base_url": format!("{}/v1/chat/completions", upstream.uri()),
            "api_key": "sk", "upstream_model": "m" }))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "failover-pool", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/failover-pool/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "a", "priority": 1 }))
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/failover-pool/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "b", "priority": 2 }))
        .send()
        .await
        .unwrap();

    sqlx::query(
        "UPDATE pool_members SET dataset_logging_override = 0 WHERE pool_id = 'failover-pool' AND provider_id = 'a'",
    )
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE pool_members SET dataset_logging_override = 1 WHERE pool_id = 'failover-pool' AND provider_id = 'b'",
    )
    .execute(&app.db)
    .await
    .unwrap();
    // Force a snapshot reload to pick up the raw writes above - any
    // provider/pool mutation endpoint does this; a no-op patch is the
    // least invasive.
    patch_provider(&app, "a", json!({})).await;

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(k, v)
        .json(&json!({ "model": "failover-pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    wait_for_writer().await;
    assert!(read_records(&app, "a").await.is_empty(), "member A never wins, must never be logged");
    let records_b = read_records(&app, "b").await;
    assert_eq!(records_b.len(), 1, "member B's override must fire even though it wasn't the first attempt");
    assert_eq!(records_b[0]["pool_id"], "failover-pool");
}
