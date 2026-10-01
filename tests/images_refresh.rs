//! 401 → refresh → retry the same account. Its own test binary: the Codex
//! token endpoint is only overridable through the process-global
//! `CODEX_TOKEN_URL` env var.

mod common;
use common::{auth_header, spawn_app_with_config};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HAPPY: &str = include_str!("fixtures/codex_image_happy.sse");

#[tokio::test]
async fn expired_token_is_refreshed_and_the_same_account_retried() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(HAPPY),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    std::env::set_var("CODEX_TOKEN_URL", format!("{}/oauth/token", upstream.uri()));

    let url = format!("{}/codex/responses", upstream.uri());
    let app = spawn_app_with_config(move |c| c.media.codex_responses_url = url).await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    let admin = |m: reqwest::Method, p: &str| client.request(m, format!("{}{p}", app.base_url)).header(&k, &v);

    admin(reqwest::Method::POST, "/admin/providers")
        .json(&json!({
            "id": "cx", "name": "cx", "wire_format": "openai", "kind": "oauth_codex",
            "base_url": null, "api_key": null, "upstream_model": "gpt-5.5"
        }))
        .send()
        .await
        .unwrap();
    router::providers::queries::upsert_oauth_tokens(
        &app.db,
        "cx",
        Some("at-old"),
        Some("rt-old"),
        None,
        Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        &json!({ "chatgpt_account_id": "acct-cx" }),
    )
    .await
    .unwrap();
    admin(reqwest::Method::POST, "/admin/pools")
        .json(&json!({ "id": "img", "wire_format": "openai", "modality": "image" }))
        .send()
        .await
        .unwrap();
    admin(reqwest::Method::PUT, "/admin/pools/img/members")
        .json(&json!({ "provider_id": "cx", "priority": 1, "model_override": "gpt-image-2" }))
        .send()
        .await
        .unwrap();
    admin(reqwest::Method::PATCH, "/admin/settings/images")
        .json(&json!({ "images_enabled": true }))
        .send()
        .await
        .unwrap();

    let resp = admin(reqwest::Method::POST, "/v1/images/generations")
        .json(&json!({ "model": "img", "prompt": "a fox" }))
        .send()
        .await
        .unwrap();
    std::env::remove_var("CODEX_TOKEN_URL");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-1router-tried"], "cx");

    let os = router::providers::queries::get_oauth_state(&app.db, "cx").await.unwrap().unwrap();
    assert_eq!(os.access_token.as_deref(), Some("at-new"));
    assert_eq!(os.refresh_token.as_deref(), Some("rt-new"));
}
