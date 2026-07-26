mod common;
use common::{auth_header, spawn_app};
use serde_json::json;

// These tests run against REAL provider APIs and cost money / can be rate-limited.
// They are excluded from the fast loop via #[ignore] and must be run explicitly:
//   cargo test --test e2e_real_providers -- --ignored
//
// Requires env: E2E_OPENAI_KEY, E2E_OPENAI_BASE, E2E_ANTHROPIC_KEY, E2E_ANTHROPIC_BASE.

#[tokio::test]
#[ignore = "real-provider e2e; run manually with sample keys"]
async fn openai_passthrough_real() {
    let key = std::env::var("E2E_OPENAI_KEY").expect("E2E_OPENAI_KEY");
    let base = std::env::var("E2E_OPENAI_BASE").expect("E2E_OPENAI_BASE");

    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    // PassthroughAdapter::build_request posts to `base_url` as-is (no path is
    // appended downstream), so the full upstream chat-completions path must be
    // baked in here, exactly like tests/proxy_failover.rs does against wiremock.
    let base_url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "openai-real",
            "name": "openai-real",
            "wire_format": "openai",
            "kind": "passthrough",
            "base_url": base_url,
            "api_key": key,
            "upstream_model": "gpt-4o-mini"
        }))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-real", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();

    client
        .put(format!("{}/admin/pools/gpt-real/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "openai-real", "priority": 1 }))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "model": "gpt-real",
            "messages": [{ "role": "user", "content": "Say OK and nothing else." }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let choices = body["choices"].as_array().expect("choices array");
    assert!(!choices.is_empty(), "expected at least one choice");
    let content = choices[0]["message"]["content"]
        .as_str()
        .expect("choices[0].message.content should be a string");
    assert!(!content.is_empty(), "expected non-empty message content");
}

#[tokio::test]
#[ignore = "real-provider e2e; run manually with sample keys"]
async fn anthropic_passthrough_real() {
    let key = std::env::var("E2E_ANTHROPIC_KEY").expect("E2E_ANTHROPIC_KEY");
    let base = std::env::var("E2E_ANTHROPIC_BASE").expect("E2E_ANTHROPIC_BASE");
    let _ = (key, base);
    // Same shape via /v1/messages against a real Anthropic-compatible upstream.
    unimplemented!("fill in when sample keys are provided");
}

#[tokio::test]
#[ignore = "real-provider e2e; failover with an intentionally invalid key first"]
async fn failover_real() {
    // Pool: [invalid-key provider @priority 1, valid provider @priority 2];
    // assert the request still succeeds via the second provider.
    unimplemented!("fill in when sample keys are provided");
}

// Requires manual browser interaction (a real ChatGPT login) and must be run
// with --nocapture so the authorize URL and prompts are visible:
//   cargo test --test e2e_real_providers codex_end_to_end_real -- --ignored --nocapture
#[tokio::test]
#[ignore = "real-provider e2e; Codex against a real ChatGPT account"]
async fn codex_end_to_end_real() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "cx-real",
            "name": "cx-real",
            "wire_format": "openai",
            "kind": "oauth_codex",
            "base_url": null,
            "api_key": null,
            "upstream_model": "gpt-5-codex"
        }))
        .send()
        .await
        .unwrap();

    let start_resp: serde_json::Value = client
        .post(format!(
            "{}/admin/providers/cx-real/oauth/start",
            app.base_url
        ))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let authorize_url = start_resp["authorize_url"]
        .as_str()
        .expect("authorize_url should be a string")
        .to_string();

    println!(
        "\n=== Codex OAuth: open this URL in your browser, log in, then paste the FULL \
        redirect URL (or just the `code=...&state=...` query params) below ===\n{authorize_url}\n"
    );
    eprintln!(
        "=== Codex OAuth authorize URL (visible even if stdout is captured) ===\n{authorize_url}\n\
        Paste the redirect URL or code=...&state=... below and press Enter:"
    );

    let line = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).unwrap();
        s
    })
    .await
    .unwrap();

    let query = line.trim().split_once('?').map(|(_, q)| q).unwrap_or(line.trim());
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => code = Some(value.to_string()),
                "state" => state = Some(value.to_string()),
                _ => {}
            }
        }
    }
    let code = code.expect("could not parse `code` from pasted input");
    let state = state.expect("could not parse `state` from pasted input");

    let resp = client
        .post(format!(
            "{}/admin/providers/cx-real/oauth/complete",
            app.base_url
        ))
        .header(&k, &v)
        .json(&json!({ "code": code, "state": state }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "cx-real-pool", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();

    client
        .put(format!("{}/admin/pools/cx-real-pool/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "cx-real", "priority": 1 }))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "model": "cx-real-pool",
            "messages": [{ "role": "user", "content": "Say OK and nothing else." }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let choices = body["choices"].as_array().expect("choices array");
    assert!(!choices.is_empty(), "expected at least one choice");
    let content = choices[0]["message"]["content"]
        .as_str()
        .expect("choices[0].message.content should be a string");
    assert!(!content.is_empty(), "expected non-empty message content");
}
