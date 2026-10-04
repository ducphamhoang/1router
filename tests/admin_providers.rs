mod common;
use common::{auth_header, spawn_app};
use serde_json::json;

#[tokio::test]
async fn create_list_and_mask_api_key() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let create = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk-supersecret", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 201);
    let body: serde_json::Value = create.json().await.unwrap();
    assert_ne!(body["api_key"], "sk-supersecret"); // masked
    assert!(body["api_key"].as_str().unwrap().contains("***"));

    let list = client
        .get(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    let arr: serde_json::Value = list.json().await.unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert!(arr[0]["api_key"].as_str().unwrap().contains("***"));
}

#[tokio::test]
async fn create_provider_accepts_dataset_logging() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let create = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk", "upstream_model": "gpt-4o", "dataset_logging": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 201);
    let body: serde_json::Value = create.json().await.unwrap();
    assert_eq!(body["dataset_logging"], true);

    // Absent entirely -> defaults false, not rejected.
    let create2 = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p2", "name": "P2", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create2.status(), 201);
    let body2: serde_json::Value = create2.json().await.unwrap();
    assert_eq!(body2["dataset_logging"], false);
}

#[tokio::test]
async fn patch_provider_updates_dataset_logging() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();

    let patch = client
        .patch(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .json(&json!({ "dataset_logging": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);

    let get: serde_json::Value = client
        .get(format!("{}/admin/providers/p1", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get["dataset_logging"], true);
}

#[tokio::test]
async fn get_missing_provider_is_404() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/nope", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// Regression test for the axum path-param syntax bug (`{id}` vs `:id`) caught
// by the Phase 1 review: without this, get_missing_provider_is_404 above and
// the export/import roundtrip test both passed for the wrong reason, because
// EVERY /admin/providers/:id request 404'd, existing or not.
#[tokio::test]
async fn get_patch_delete_existing_provider_by_id_succeeds() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk-secret", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();

    let get = client
        .get(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 200);

    let patch = client
        .patch(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .json(&json!({ "name": "P1 renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);

    let delete = client
        .delete(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), 204);

    let get_after = client
        .get(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(get_after.status(), 404);
}

#[tokio::test]
async fn provider_response_reports_credential_configured_once_the_commandcode_key_is_saved() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let created: serde_json::Value = client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"cc-1"})).send().await.unwrap().json().await.unwrap();
    assert_eq!(created["credential_configured"], false);

    let before = client
        .get(format!("{}/admin/providers/cc", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(before["credential_configured"], false);

    client
        .post(format!("{}/admin/providers/cc/commandcode/key", app.base_url))
        .header(&k, &v)
        .json(&json!({"api_key":"cc-secret"}))
        .send()
        .await
        .unwrap();

    let after = client
        .get(format!("{}/admin/providers/cc", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(after["credential_configured"], true);

    let list = client
        .get(format!("{}/admin/providers", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(list[0]["credential_configured"], true);
}

#[tokio::test]
async fn commandcode_key_endpoint_stores_the_key() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"cc-1"})).send().await.unwrap();
    let response = client
        .post(format!(
            "{}/admin/providers/cc/commandcode/key",
            app.base_url
        ))
        .header(&k, &v)
        .json(&json!({"api_key":"cc-secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["ok"],
        true
    );
    let state = router::providers::queries::get_oauth_state(&app.db, "cc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.access_token.as_deref(), Some("cc-secret"));
    assert_eq!(state.refresh_token.as_deref(), Some("cc-secret"));
    let provider = client
        .get(format!("{}/admin/providers/cc", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(provider["api_key"].is_null());
}

#[tokio::test]
async fn commandcode_key_endpoint_rejects_a_non_commandcode_provider() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"p","name":"p","wire_format":"openai","kind":"passthrough","base_url":"http://127.0.0.1:1","api_key":"k","upstream_model":"m"})).send().await.unwrap();
    let response = client
        .post(format!(
            "{}/admin/providers/p/commandcode/key",
            app.base_url
        ))
        .header(k, v)
        .json(&json!({"api_key":"k"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn commandcode_key_endpoint_rejects_an_empty_key_when_none_on_disk() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"m"})).send().await.unwrap();

    // The empty-key path means "use the key found on this machine" (env or
    // ~/.commandcode/auth.json etc). Point the home dir at a non-existent
    // directory so the test is deterministic regardless of the runner's
    // actual auth files, and clear the env overrides.
    let empty_home = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let original = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let original = std::env::var_os("HOME");
    #[cfg(windows)]
    std::env::set_var("USERPROFILE", empty_home.path());
    #[cfg(not(windows))]
    std::env::set_var("HOME", empty_home.path());
    std::env::remove_var("COMMANDCODE_API_KEY");
    std::env::remove_var("ROUTER_COMMANDCODE_API_KEY");

    let response = client
        .post(format!(
            "{}/admin/providers/cc/commandcode/key",
            app.base_url
        ))
        .header(k, v)
        .json(&json!({"api_key":"  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);

    #[cfg(windows)]
    match original {
        Some(value) => std::env::set_var("USERPROFILE", value),
        None => std::env::remove_var("USERPROFILE"),
    }
    #[cfg(not(windows))]
    match original {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
}

#[tokio::test]
async fn commandcode_key_endpoint_uses_a_key_from_disk_when_body_key_is_empty() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"m"})).send().await.unwrap();

    let home = tempfile::tempdir().unwrap();
    let auth_dir = home.path().join(".commandcode");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"apiKey":"user_disk_key"}"#,
    )
    .unwrap();
    std::env::remove_var("COMMANDCODE_API_KEY");
    std::env::remove_var("ROUTER_COMMANDCODE_API_KEY");
    #[cfg(windows)]
    let original = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let original = std::env::var_os("HOME");
    #[cfg(windows)]
    std::env::set_var("USERPROFILE", home.path());
    #[cfg(not(windows))]
    std::env::set_var("HOME", home.path());

    let response = client
        .post(format!(
            "{}/admin/providers/cc/commandcode/key",
            app.base_url
        ))
        .header(k, v)
        .json(&json!({"api_key":""}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let state = sqlx::query_as::<_, router::core::model::OAuthState>(
        "SELECT * FROM provider_oauth_state WHERE provider_id = 'cc'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(state.access_token.as_deref(), Some("user_disk_key"));

    #[cfg(windows)]
    match original {
        Some(value) => std::env::set_var("USERPROFILE", value),
        None => std::env::remove_var("USERPROFILE"),
    }
    #[cfg(not(windows))]
    match original {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
}

#[tokio::test]
async fn create_provider_accepts_the_new_kind() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let response = client.post(format!("{}/admin/providers", app.base_url)).header(k, v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"m"})).send().await.unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["kind"],
        "oauth_command_code"
    );
}

#[tokio::test]
async fn commandcode_browser_login_start_rejects_a_non_commandcode_provider() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"p","name":"p","wire_format":"openai","kind":"passthrough","base_url":"http://127.0.0.1:1","api_key":"k","upstream_model":"m"})).send().await.unwrap();
    let response = client
        .post(format!(
            "{}/admin/providers/p/commandcode/browser-login/start",
            app.base_url
        ))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn commandcode_browser_login_status_is_not_started_for_an_unknown_provider() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let response = client
        .get(format!(
            "{}/admin/providers/nope/commandcode/browser-login/status",
            app.base_url
        ))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["status"],
        "not_started"
    );
}

fn parse_authorize_url_query(url: &str) -> (String, String) {
    let query = url.split('?').nth(1).expect("authorize_url has a query string");
    let mut callback = String::new();
    let mut state = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').expect("query pair has a value");
        let decoded = urlencoding::decode(value).unwrap().into_owned();
        match key {
            "callback" => callback = decoded,
            "state" => state = decoded,
            _ => {}
        }
    }
    (callback, state)
}

#[tokio::test]
async fn commandcode_browser_login_completes_end_to_end() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client.post(format!("{}/admin/providers", app.base_url)).header(&k, &v).json(&json!({"id":"cc","name":"cc","wire_format":"openai","kind":"oauth_command_code","upstream_model":"m"})).send().await.unwrap();

    let start = client
        .post(format!(
            "{}/admin/providers/cc/commandcode/browser-login/start",
            app.base_url
        ))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 200);
    let start_body: serde_json::Value = start.json().await.unwrap();
    let authorize_url = start_body["authorize_url"].as_str().unwrap();

    let pending = client
        .get(format!(
            "{}/admin/providers/cc/commandcode/browser-login/status",
            app.base_url
        ))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");

    let (callback_url, state) = parse_authorize_url_query(authorize_url);
    let port = reqwest::Url::parse(&callback_url).unwrap().port().unwrap();

    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/callback"))
        .json(&json!({"apiKey":"cc-secret","state":state,"userId":"u1","userName":"User","keyName":"cli"}))
        .send()
        .await
        .unwrap();

    let mut status = json!({"status": "pending"});
    for _ in 0..50 {
        status = client
            .get(format!(
                "{}/admin/providers/cc/commandcode/browser-login/status",
                app.base_url
            ))
            .header(&k, &v)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["status"] != "pending" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(status["status"], "success");

    let state_row = router::providers::queries::get_oauth_state(&app.db, "cc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state_row.access_token.as_deref(), Some("cc-secret"));

    let followup = client
        .get(format!(
            "{}/admin/providers/cc/commandcode/browser-login/status",
            app.base_url
        ))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(followup["status"], "not_started");
}

async fn post_provider(
    app: &common::TestApp,
    body: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/providers", app.base_url))
        .header(k, v)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

#[tokio::test]
async fn precreated_providers_report_readiness_and_credential_status() {
    let app = spawn_app().await;

    let (st, codex) = post_provider(
        &app,
        json!({ "id": "cx", "name": "Codex", "wire_format": "openai",
                "kind": "oauth_codex", "base_url": "", "api_key": "", "upstream_model": "gpt-5" }),
    )
    .await;
    assert_eq!(st, 201);
    assert_eq!(codex["ready"], false);
    assert_eq!(codex["credential_status"], "not_connected");
    assert!(codex["base_url"].is_null(), "empty string must normalize to null");

    let (_, keyless) = post_provider(
        &app,
        json!({ "id": "k", "name": "Keyless", "wire_format": "openai",
                "base_url": "https://x.test/v1/chat/completions/", "upstream_model": "m" }),
    )
    .await;
    assert_eq!(keyless["ready"], true);
    assert_eq!(keyless["credential_status"], "none");
    assert_eq!(keyless["base_url"], "https://x.test/v1/chat/completions");
}

#[tokio::test]
async fn create_validates_input_and_distinguishes_duplicates() {
    let app = spawn_app().await;
    let ok = json!({ "id": "p1", "name": "P1", "wire_format": "openai",
                     "base_url": "https://x.test", "upstream_model": "m" });
    assert_eq!(post_provider(&app, ok.clone()).await.0, 201);

    let mut bad_id = ok.clone();
    bad_id["id"] = json!("has space/slash");
    assert_eq!(post_provider(&app, bad_id).await.0, 400);

    let mut bad_url = ok.clone();
    bad_url["id"] = json!("p2");
    bad_url["name"] = json!("P2");
    bad_url["base_url"] = json!("ftp://x");
    assert_eq!(post_provider(&app, bad_url).await.0, 400);

    let mut dup_id = ok.clone();
    dup_id["name"] = json!("Other");
    let (st, body) = post_provider(&app, dup_id).await;
    assert_eq!(st, 409);
    assert!(body["error"]["message"].as_str().unwrap().contains("id"));

    let mut dup_name = ok.clone();
    dup_name["id"] = json!("p9");
    let (st, body) = post_provider(&app, dup_name).await;
    assert_eq!(st, 409);
    assert!(body["error"]["message"].as_str().unwrap().contains("name"));
}

#[tokio::test]
async fn patch_null_clears_api_key_and_blank_leaves_via_omission() {
    let app = spawn_app().await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    post_provider(
        &app,
        json!({ "id": "p1", "name": "P1", "wire_format": "openai",
                "base_url": "https://x.test", "api_key": "sk-long-secret-1234", "upstream_model": "m" }),
    )
    .await;
    let patch = |body: serde_json::Value| {
        client
            .patch(format!("{}/admin/providers/p1", app.base_url))
            .header(&k, &v)
            .json(&body)
            .send()
    };
    let kept: serde_json::Value = patch(json!({ "name": "P1b" })).await.unwrap().json().await.unwrap();
    assert_eq!(kept["credential_status"], "set");
    assert_eq!(kept["api_key"], "***1234");
    let cleared: serde_json::Value =
        patch(json!({ "api_key": null })).await.unwrap().json().await.unwrap();
    assert_eq!(cleared["credential_status"], "none");
    assert!(cleared["api_key"].is_null());
}

#[tokio::test]
async fn disconnect_oauth_only_for_codex() {
    let app = spawn_app().await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    post_provider(
        &app,
        json!({ "id": "cx", "name": "Codex", "wire_format": "openai",
                "kind": "oauth_codex", "upstream_model": "gpt-5" }),
    )
    .await;
    post_provider(
        &app,
        json!({ "id": "p1", "name": "P1", "wire_format": "openai",
                "base_url": "https://x.test", "upstream_model": "m" }),
    )
    .await;
    let del = |id: &str| {
        client
            .delete(format!("{}/admin/providers/{id}/oauth", app.base_url))
            .header(&k, &v)
            .send()
    };
    assert_eq!(del("cx").await.unwrap().status(), 204);
    assert_eq!(del("p1").await.unwrap().status(), 400);
    assert_eq!(del("nope").await.unwrap().status(), 404);
}

#[tokio::test]
async fn draft_test_classifies_upstream_responses() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/denied"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&upstream)
        .await;

    let app = spawn_app().await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    let probe = |p: &str| {
        client
            .post(format!("{}/admin/provider-test", app.base_url))
            .header(&k, &v)
            .json(&json!({ "wire_format": "openai", "base_url": format!("{}{p}", upstream.uri()),
                           "api_key": "sk-x", "upstream_model": "m" }))
            .send()
    };
    let ok: serde_json::Value = probe("/ok").await.unwrap().json().await.unwrap();
    assert_eq!(ok["ok"], true);
    let denied: serde_json::Value = probe("/denied").await.unwrap().json().await.unwrap();
    assert_eq!(denied["category"], "auth");
    let missing: serde_json::Value = probe("/nowhere").await.unwrap().json().await.unwrap();
    assert_eq!(missing["category"], "wrong_path");
}

#[tokio::test]
async fn provider_with_id_test_is_still_addressable() {
    let app = spawn_app().await;
    let (k, v) = auth_header(&app.secret);
    post_provider(
        &app,
        json!({ "id": "test", "name": "T", "wire_format": "openai",
                "base_url": "https://x.test", "upstream_model": "m" }),
    )
    .await;
    let get = reqwest::Client::new()
        .get(format!("{}/admin/providers/test", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 200);
}

#[tokio::test]
async fn disconnect_oauth_really_removes_tokens() {
    let app = spawn_app().await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    post_provider(
        &app,
        json!({ "id": "cx", "name": "Codex", "wire_format": "openai",
                "kind": "oauth_codex", "upstream_model": "gpt-5" }),
    )
    .await;
    router::providers::queries::upsert_oauth_tokens(
        &app.db, "cx", Some("at"), Some("rt"), None, None, &json!({}),
    )
    .await
    .unwrap();
    let get = || async {
        client
            .get(format!("{}/admin/providers/cx", app.base_url))
            .header(&k, &v)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };
    let before = get().await;
    assert_eq!(before["credential_status"], "connected");
    assert_eq!(before["ready"], true);
    let del = client
        .delete(format!("{}/admin/providers/cx/oauth", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    let after = get().await;
    assert_eq!(after["credential_status"], "not_connected");
    assert_eq!(after["ready"], false);
}
