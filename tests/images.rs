//! `/v1/images/generations` end to end against a wiremock'd Codex
//! `/responses` endpoint, using SSE fixtures recorded in the P0 spike
//! (`tests/fixtures/codex_image_*.sse`, identifiers stripped).

mod common;
use common::{auth_header, spawn_app_with_config, TestApp};
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HAPPY: &str = include_str!("fixtures/codex_image_happy.sse");
const WITH_TEXT: &str = include_str!("fixtures/codex_image_with_text.sse");
const ALIAS: &str = include_str!("fixtures/codex_image_alias.sse");

fn ev(v: Value) -> String {
    format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v)
}

fn message(text: &str) -> String {
    ev(json!({"type": "response.output_item.done", "item": {
        "type": "message", "content": [{"type": "output_text", "text": text}]}}))
}

fn completed() -> String {
    ev(json!({"type": "response.completed", "response": {"output": []}}))
}

fn sse(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body.into())
}

async fn spawn(upstream: &MockServer, tweak: impl FnOnce(&mut router::core::config::Config)) -> TestApp {
    let url = format!("{}/codex/responses", upstream.uri());
    spawn_app_with_config(move |cfg| {
        cfg.media.codex_responses_url = url;
        tweak(cfg);
    })
    .await
}

fn admin(app: &TestApp, m: reqwest::Method, p: &str) -> reqwest::RequestBuilder {
    let (k, v) = auth_header(&app.secret);
    reqwest::Client::new().request(m, format!("{}{p}", app.base_url)).header(k, v)
}

async fn set_images(app: &TestApp, on: bool) {
    let r = admin(app, reqwest::Method::PATCH, "/admin/settings/images")
        .json(&json!({ "images_enabled": on }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

async fn codex_provider(app: &TestApp, id: &str) {
    let r = admin(app, reqwest::Method::POST, "/admin/providers")
        .json(&json!({
            "id": id, "name": id, "wire_format": "openai", "kind": "oauth_codex",
            "base_url": null, "api_key": null, "upstream_model": "gpt-5.5"
        }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    router::providers::queries::upsert_oauth_tokens(
        &app.db,
        id,
        Some(&format!("at-{id}")),
        Some(&format!("rt-{id}")),
        None,
        Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        &json!({ "chatgpt_account_id": format!("acct-{id}") }),
    )
    .await
    .unwrap();
}

async fn pool(app: &TestApp, id: &str, modality: &str) {
    let r = admin(app, reqwest::Method::POST, "/admin/pools")
        .json(&json!({ "id": id, "wire_format": "openai", "modality": modality }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
}

async fn member(app: &TestApp, pool_id: &str, provider: &str, priority: i64, model: &str) -> reqwest::Response {
    admin(app, reqwest::Method::PUT, &format!("/admin/pools/{pool_id}/members"))
        .json(&json!({ "provider_id": provider, "priority": priority, "model_override": model }))
        .send()
        .await
        .unwrap()
}

/// Two Codex accounts (`cx1` first, then `cx2`) in image pool `img`,
/// images enabled.
async fn two_account_app(upstream: &MockServer) -> TestApp {
    two_account_app_with(upstream, |_| {}).await
}

async fn two_account_app_with(
    upstream: &MockServer,
    tweak: impl FnOnce(&mut router::core::config::Config),
) -> TestApp {
    let app = spawn(upstream, tweak).await;
    codex_provider(&app, "cx1").await;
    codex_provider(&app, "cx2").await;
    pool(&app, "img", "image").await;
    assert_eq!(member(&app, "img", "cx1", 1, "gpt-image-2").await.status(), 200);
    assert_eq!(member(&app, "img", "cx2", 2, "gpt-image-2").await.status(), 200);
    set_images(&app, true).await;
    app
}

fn account(id: &str) -> wiremock::matchers::HeaderExactMatcher {
    header("ChatGPT-Account-ID", format!("acct-{id}").as_str())
}

async fn mount(upstream: &MockServer, id: &str, resp: ResponseTemplate, expect: u64) {
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(account(id))
        .respond_with(resp)
        .expect(expect)
        .mount(upstream)
        .await;
}

async fn generate(app: &TestApp, body: Value) -> reqwest::Response {
    admin(app, reqwest::Method::POST, "/v1/images/generations").json(&body).send().await.unwrap()
}

async fn generate_img(app: &TestApp) -> reqwest::Response {
    generate(app, json!({ "model": "img", "prompt": "a fox" })).await
}

async fn user_key(app: &TestApp, id: &str) -> String {
    let r: Value = admin(app, reqwest::Method::POST, "/admin/users")
        .json(&json!({ "id": id }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    r["api_key"].as_str().unwrap().to_string()
}

async fn error_code(resp: reqwest::Response) -> String {
    let v: Value = resp.json().await.unwrap();
    v["error"]["code"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn happy_path_returns_b64_and_sends_the_codex_body() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    let app = two_account_app(&upstream).await;

    let resp = generate(
        &app,
        json!({ "model": "img", "prompt": "a fox", "size": "1024x1024", "quality": "low" }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-1router-provider"], "cx1");
    let body: Value = resp.json().await.unwrap();
    assert!(body["data"][0]["b64_json"].as_str().unwrap().starts_with("iVBORw0KGgo"));
    assert!(body["data"][0]["revised_prompt"].as_str().unwrap().contains("fox"));
    assert!(body["data"][0].get("text").is_none(), "empty assistant text is dropped");
    assert_eq!(body["size"], "1254x1254", "actual upstream size, not the requested one");
    assert_eq!(body["usage"]["total_tokens"], 561);

    let reqs = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(reqs[0].headers["authorization"], "Bearer at-cx1");
    assert_eq!(reqs[0].headers["originator"], "codex_cli_rs");
    assert_eq!(sent["model"], "gpt-5.5");
    assert_eq!(sent["tool_choice"], json!({"type": "image_generation"}));
    assert_eq!(sent["tools"][0]["type"], "image_generation");
    assert_eq!(sent["tools"][0]["size"], "1024x1024");
    assert_eq!(sent["reasoning"]["effort"], "medium");
    assert_eq!(sent["stream"], true);
    assert!(sent["input"][0]["content"][0]["text"].as_str().unwrap().contains("size: 1024x1024"));

    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows: Vec<(Option<String>, Option<f64>, bool)> =
        sqlx::query_as("SELECT modality, units, success FROM request_log").fetch_all(&app.db).await.unwrap();
    assert_eq!(rows, vec![(Some("image".into()), Some(1.0), true)]);
}

#[tokio::test]
async fn image_alias_uses_its_chat_model_and_no_reasoning() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(ALIAS), 1).await;
    let app = spawn(&upstream, |_| {}).await;
    codex_provider(&app, "cx1").await;
    pool(&app, "img", "image").await;
    assert_eq!(member(&app, "img", "cx1", 1, "gpt-5.5-image").await.status(), 200);
    set_images(&app, true).await;

    assert_eq!(generate_img(&app).await.status(), 200);
    let reqs = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-5.5");
    assert_eq!(sent["tool_choice"], "auto");
    assert!(sent.get("reasoning").is_none());
    assert!(sent["tools"][0].get("model").is_none());
}

#[tokio::test]
async fn text_next_to_an_image_is_returned_not_treated_as_refusal() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(WITH_TEXT), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 0).await;
    let app = two_account_app(&upstream).await;
    let body: Value = generate_img(&app).await.json().await.unwrap();
    assert!(body["data"][0]["text"].as_str().unwrap().starts_with("Sorry"));
    assert!(body["data"][0]["b64_json"].is_string());
}

#[tokio::test]
async fn invalid_requests_are_rejected_before_any_upstream_call() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 0).await;
    let app = two_account_app(&upstream).await;
    for bad in [
        json!({ "model": "img", "prompt": "x", "n": 2 }),
        json!({ "model": "img", "prompt": "x", "response_format": "url" }),
        json!({ "model": "img", "prompt": "x", "stream": true }),
        json!({ "model": "img", "prompt": "" }),
        json!({ "model": "img", "prompt": "x", "size": "7x7" }),
        json!({ "model": "img", "prompt": "x", "quality": "ultra" }),
        json!({ "model": "nope", "prompt": "x" }),
    ] {
        assert_eq!(generate(&app, bad.clone()).await.status(), 400, "{bad}");
    }
}

#[tokio::test]
async fn usage_limit_429_fails_over_and_cools_down_from_the_body() {
    let upstream = MockServer::start().await;
    mount(
        &upstream,
        "cx1",
        ResponseTemplate::new(429)
            .set_body_json(json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}})),
        1,
    )
    .await;
    mount(&upstream, "cx2", sse(HAPPY), 2).await;
    let app = two_account_app(&upstream).await;
    let first = generate_img(&app).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-1router-tried"], "cx1,cx2");
    // cx1 is cooling: the next request goes straight to cx2.
    assert_eq!(generate_img(&app).await.headers()["x-1router-tried"], "cx2");
}

#[tokio::test]
async fn sse_usage_limit_error_fails_over() {
    let upstream = MockServer::start().await;
    let failed = ev(json!({"type": "response.failed", "response": {"error": {
        "code": "usage_limit_reached", "message": "limit reached"}}}));
    mount(&upstream, "cx1", sse(failed), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 1).await;
    let app = two_account_app(&upstream).await;
    assert_eq!(generate_img(&app).await.status(), 200);
}

#[tokio::test]
async fn server_errors_404_and_unsupported_host_model_fail_over() {
    for resp in [
        ResponseTemplate::new(503),
        ResponseTemplate::new(404),
        ResponseTemplate::new(400).set_body_json(json!({
            "detail": "The 'gpt-5.5' model is not supported when using Codex with a ChatGPT account."})),
    ] {
        let upstream = MockServer::start().await;
        mount(&upstream, "cx1", resp, 1).await;
        mount(&upstream, "cx2", sse(HAPPY), 1).await;
        let app = two_account_app(&upstream).await;
        assert_eq!(generate_img(&app).await.status(), 200);
    }
}

#[tokio::test]
async fn a_bad_request_upstream_is_relayed_without_failover() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", ResponseTemplate::new(400).set_body_json(json!({"detail": "bad size"})), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 0).await;
    let app = two_account_app(&upstream).await;
    let resp = generate_img(&app).await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["message"], "bad size");
}

#[tokio::test]
async fn completed_without_image_or_text_fails_over_and_marks_misconfigured() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(message("") + &completed()), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 2).await;
    let app = two_account_app(&upstream).await;
    assert_eq!(generate_img(&app).await.status(), 200);
    assert_eq!(generate_img(&app).await.headers()["x-1router-tried"], "cx2");
}

#[tokio::test]
async fn refusal_moderation_and_truncation_do_not_fail_over() {
    let moderation = ev(json!({"type": "error", "code": "moderation_blocked", "message": "blocked"}));
    for (body, status, code) in [
        (message("I can't help with that.") + &completed(), 400, "image_generation_refused"),
        (moderation, 400, "moderation_blocked"),
        // Cut before `response.completed`: not the account's fault.
        (message("partial"), 502, "upstream_stream_incomplete"),
    ] {
        let upstream = MockServer::start().await;
        mount(&upstream, "cx1", sse(body), 2).await;
        mount(&upstream, "cx2", sse(HAPPY), 0).await;
        let app = two_account_app(&upstream).await;
        let resp = generate_img(&app).await;
        assert_eq!(resp.status(), status);
        assert_eq!(error_code(resp).await, code);
        // Still healthy: the next request tries cx1 again.
        assert_eq!(generate_img(&app).await.headers()["x-1router-tried"], "cx1");
    }
}

#[tokio::test]
async fn response_cap_trips_as_502() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 0).await;
    let app = two_account_app_with(&upstream, |c| c.media.max_response_bytes = 1024).await;
    let resp = generate_img(&app).await;
    assert_eq!(resp.status(), 502);
    assert_eq!(error_code(resp).await, "upstream_stream_incomplete");
}

#[tokio::test]
async fn a_slow_generation_outlives_the_chat_idle_timeout() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY).set_delay(Duration::from_millis(1500)), 1).await;
    let app = two_account_app_with(&upstream, |c| {
        c.idle_timeout = Duration::from_millis(500);
        c.ttfb_timeout = Duration::from_millis(500);
    })
    .await;
    assert_eq!(generate_img(&app).await.status(), 200);
}

#[tokio::test]
async fn request_timeout_is_504_without_failover() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY).set_delay(Duration::from_secs(3)), 1).await;
    mount(&upstream, "cx2", sse(HAPPY), 0).await;
    let app = two_account_app_with(&upstream, |c| c.media.request_timeout = Duration::from_millis(500)).await;
    assert_eq!(generate_img(&app).await.status(), 504);
}

#[tokio::test]
async fn concurrency_cap_returns_429() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY).set_delay(Duration::from_millis(1500)), 1).await;
    let app = two_account_app_with(&upstream, |c| c.media.max_concurrency = 1).await;
    let slow = generate_img(&app);
    let second = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        generate_img(&app).await
    };
    let (a, b) = tokio::join!(slow, second);
    assert_eq!(a.status(), 200);
    assert_eq!(b.status(), 429);
}

#[tokio::test]
async fn disabled_is_404_and_anonymous_open_access_is_401() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 0).await;
    let app = two_account_app(&upstream).await;

    set_images(&app, false).await;
    assert_eq!(generate_img(&app).await.status(), 404);
    let models: Value = admin(&app, reqwest::Method::GET, "/v1/models").send().await.unwrap().json().await.unwrap();
    assert!(!models["data"].as_array().unwrap().iter().any(|m| m["id"] == "img"));

    set_images(&app, true).await;
    let r = admin(&app, reqwest::Method::PATCH, "/admin/settings/auth-mode")
        .json(&json!({ "require_shared_secret": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let anon = reqwest::Client::new()
        .post(format!("{}/v1/images/generations", app.base_url))
        .json(&json!({ "model": "img", "prompt": "a fox" }))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let anon_edit = reqwest::Client::new()
        .post(format!("{}/v1/images/edits", app.base_url))
        .json(&json!({ "model": "img", "prompt": "a fox", "images": [PNG_DATA_URL] }))
        .send()
        .await
        .unwrap();
    assert_eq!(anon_edit.status(), 401);
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-png-body";
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0fake-jpeg-body";
// base64 of PNG.
const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgpmYWtlLXBuZy1ib2R5";

/// A hand-built multipart body (the test reqwest has no `multipart` feature).
/// Parts are `(name, filename, bytes)`; `None` filename = a text field.
fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "1router-test-boundary";
    let mut body = Vec::new();
    for (name, filename, bytes) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            ),
            None => body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
        }
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

async fn edit_multipart(app: &TestApp, parts: &[(&str, Option<&str>, &[u8])]) -> reqwest::Response {
    let (content_type, body) = multipart(parts);
    admin(app, reqwest::Method::POST, "/v1/images/edits")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap()
}

fn sent_body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

#[tokio::test]
async fn multipart_edit_sends_reference_images_before_the_prompt() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    let app = two_account_app(&upstream).await;

    let resp = edit_multipart(
        &app,
        &[
            ("model", None, b"img"),
            ("prompt", None, b"redraw image1 in the style of image2"),
            ("n", None, b"1"),
            ("size", None, b"1024x1536"),
            ("background", None, b"transparent"),
            ("input_fidelity", None, b"high"),
            ("image[]", Some("subject.png"), PNG),
            ("image[]", Some("style.jpg"), JPEG),
        ],
    )
    .await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let reqs = upstream.received_requests().await.unwrap();
    let sent = sent_body(&reqs[0]);
    assert_eq!(sent["tools"][0]["action"], "edit");
    assert!(sent["tools"][0].get("background").is_none(), "transparent is a prompt hint only");
    let content = sent["input"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 7);
    assert_eq!(content[0]["text"], "<image name=image1>");
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], PNG_DATA_URL);
    assert!(content[4]["image_url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    let prompt = content[6]["text"].as_str().unwrap();
    assert!(prompt.starts_with("redraw image1") && prompt.contains("background: transparent"), "{prompt}");
}

#[tokio::test]
async fn json_edit_accepts_data_urls_and_bare_base64() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 2).await;
    let app = two_account_app(&upstream).await;

    let resp = admin(&app, reqwest::Method::POST, "/v1/images/edits")
        .json(&json!({ "model": "img", "prompt": "a fox sticker",
            "images": [{ "image_url": PNG_DATA_URL }, "iVBORw0KGgpmYWtlLXBuZy1ib2R5"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let reqs = upstream.received_requests().await.unwrap();
    let content = sent_body(&reqs[0])["input"][0]["content"].as_array().unwrap().clone();
    assert_eq!(content[1]["image_url"], PNG_DATA_URL);
    assert_eq!(content[4]["image_url"], PNG_DATA_URL, "bare base64 is sniffed and wrapped");

    // Text-only generations are unchanged.
    assert_eq!(generate_img(&app).await.status(), 200);
    let reqs = upstream.received_requests().await.unwrap();
    assert_eq!(sent_body(&reqs[1])["tools"][0]["action"], "generate");
}

#[tokio::test]
async fn bad_edits_are_rejected_before_any_upstream_call() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 0).await;
    let app = two_account_app(&upstream).await;

    let gif: &[u8] = b"GIF89a....";
    for parts in [
        vec![("model", None, &b"img"[..]), ("prompt", None, b"x")],
        vec![("model", None, b"img"), ("prompt", None, b"x"), ("image", Some("a.gif"), gif)],
        vec![("model", None, b"img"), ("prompt", None, b"x"), ("image", Some("a.png"), PNG), ("mask", Some("m.png"), PNG)],
        vec![("model", None, b"img"), ("prompt", None, b"x"), ("n", None, b"2"), ("image", Some("a.png"), PNG)],
        vec![("model", None, b"img"), ("image", Some("a.png"), PNG)],
    ] {
        let resp = edit_multipart(&app, &parts).await;
        assert_eq!(resp.status(), 400, "{:?}", parts.iter().map(|p| p.0).collect::<Vec<_>>());
    }
    let seventeen: Vec<(&str, Option<&str>, &[u8])> = [("model", None, &b"img"[..]), ("prompt", None, b"x")]
        .into_iter()
        .chain(std::iter::repeat(("image[]", Some("a.png"), PNG)).take(17))
        .collect();
    assert_eq!(edit_multipart(&app, &seventeen).await.status(), 400);

    for bad in [
        json!({ "model": "img", "prompt": "x" }),
        json!({ "model": "img", "prompt": "x", "images": ["https://example.com/a.png"] }),
        json!({ "model": "img", "prompt": "x", "images": ["not base64!"] }),
        json!({ "model": "img", "prompt": "x", "images": [PNG_DATA_URL], "mask": PNG_DATA_URL }),
        json!({ "model": "img", "prompt": "x", "images": [{ "file_id": "file-1" }] }),
    ] {
        let resp = admin(&app, reqwest::Method::POST, "/v1/images/edits").json(&bad).send().await.unwrap();
        assert_eq!(resp.status(), 400, "{bad}");
    }
}

#[tokio::test]
async fn edit_bodies_above_the_generations_cap_are_accepted() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    let app = two_account_app_with(&upstream, |cfg| cfg.max_body_bytes = 64 * 1024).await;

    let mut big = PNG.to_vec();
    big.resize(3 * 1024 * 1024, 0);
    let resp = edit_multipart(&app, &[("model", None, b"img"), ("prompt", None, b"x"), ("image", Some("a.png"), &big)]).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
}

#[tokio::test]
async fn direct_addressing_is_admin_only_and_never_leaks_into_chat() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    let app = two_account_app(&upstream).await;
    let key = user_key(&app, "alice").await;
    let as_user = |p: &str, body: Value| {
        reqwest::Client::new()
            .post(format!("{}{p}", app.base_url))
            .bearer_auth(&key)
            .json(&body)
            .send()
    };

    let r = as_user("/v1/images/generations", json!({ "model": "cx1/gpt-image-2", "prompt": "x" })).await.unwrap();
    assert_eq!(r.status(), 400);
    // A user can call the pool.
    let r = as_user("/v1/images/generations", json!({ "model": "img", "prompt": "x" })).await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("x-1router-provider").is_none(), "debug headers are admin-only");
    // The image pool's member model isn't a chat direct-addressing allowlist entry.
    let r = as_user("/v1/chat/completions", json!({ "model": "cx1/gpt-image-2", "messages": [] })).await.unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn admin_can_address_a_codex_image_model_directly() {
    let upstream = MockServer::start().await;
    mount(&upstream, "cx1", sse(HAPPY), 1).await;
    let app = two_account_app(&upstream).await;
    let r = generate(&app, json!({ "model": "cx1/gpt-image-2", "prompt": "x" })).await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn pools_are_not_interchangeable_between_chat_and_image_routes() {
    let upstream = MockServer::start().await;
    let app = two_account_app(&upstream).await;
    pool(&app, "chat", "chat").await;
    assert_eq!(member(&app, "chat", "cx1", 1, "gpt-5.5").await.status(), 200);

    let r = admin(&app, reqwest::Method::POST, "/v1/chat/completions")
        .json(&json!({ "model": "img", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(generate(&app, json!({ "model": "chat", "prompt": "x" })).await.status(), 400);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn image_pool_members_must_be_codex_with_an_image_model() {
    let upstream = MockServer::start().await;
    let app = two_account_app(&upstream).await;
    let r = admin(&app, reqwest::Method::POST, "/admin/providers")
        .json(&json!({
            "id": "pt", "name": "pt", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1/v1/chat/completions", "api_key": "sk", "upstream_model": "gpt-image-2"
        }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    assert_eq!(member(&app, "img", "pt", 3, "gpt-image-2").await.status(), 400);
    assert_eq!(member(&app, "img", "cx1", 1, "gpt-5.5").await.status(), 400);
    let no_override = admin(&app, reqwest::Method::PUT, "/admin/pools/img/members")
        .json(&json!({ "provider_id": "cx1", "priority": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(no_override.status(), 400);
}

#[tokio::test]
async fn export_import_keeps_modality_and_old_exports_import_as_chat() {
    let upstream = MockServer::start().await;
    let app = two_account_app(&upstream).await;
    let dump: Value = admin(&app, reqwest::Method::GET, "/admin/export").send().await.unwrap().json().await.unwrap();
    let img = dump["pools"].as_array().unwrap().iter().find(|p| p["id"] == "img").unwrap().clone();
    assert_eq!(img["modality"], "image");

    let mut old = dump.clone();
    let mut legacy = img.clone();
    legacy.as_object_mut().unwrap().remove("modality");
    legacy["id"] = json!("legacy");
    old["pools"] = json!([legacy]);
    old["members"] = json!([]);
    let r = admin(&app, reqwest::Method::POST, "/admin/import").json(&old).send().await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = admin(&app, reqwest::Method::POST, "/admin/import").json(&dump).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, modality FROM pools ORDER BY id").fetch_all(&app.db).await.unwrap();
    assert_eq!(rows, vec![("img".into(), "image".into()), ("legacy".into(), "chat".into())]);
}
