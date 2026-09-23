mod common;
use common::{auth_header, spawn_app};
use serde_json::{json, Value};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One passthrough provider behind pool `gpt-4o`. The mock only answers when
/// the *provider's* key (`sk-test`) is sent upstream, so every successful
/// proxy call below also proves the client's own key isn't forwarded.
async fn setup(app: &common::TestApp) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "p1", "wire_format": "openai", "kind": "passthrough",
            "base_url": upstream.uri(), "api_key": "sk-test", "upstream_model": "real-model"
        }))
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
    upstream
}

async fn create_key(app: &common::TestApp, name: &str) -> Value {
    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/client-keys", app.base_url))
        .header(k, v)
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    resp.json().await.unwrap()
}

async fn chat(app: &common::TestApp, header_name: &str, header_value: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header(header_name, header_value)
        .json(&json!({ "model": "gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap()
}

async fn logged_callers(app: &common::TestApp) -> Vec<(Option<String>, Option<String>)> {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sqlx::query_as("SELECT caller_name, caller_key_id FROM request_log ORDER BY id")
        .fetch_all(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn client_key_authenticates_and_is_logged_by_name() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_key(&app, "alice").await;
    let raw = created["api_key"].as_str().unwrap();
    let id = created["id"].as_str().unwrap();

    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 200);
    // Anthropic-style SDKs send the key as x-api-key.
    assert_eq!(chat(&app, "x-api-key", raw).await.status(), 200);

    let rows = logged_callers(&app).await;
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row, (Some("alice".into()), Some(id.to_string())));
    }
}

#[tokio::test]
async fn shared_secret_is_logged_as_admin() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    assert_eq!(chat(&app, "authorization", &format!("Bearer {}", app.secret)).await.status(), 200);
    assert_eq!(logged_callers(&app).await, vec![(Some("admin".into()), None)]);
}

#[tokio::test]
async fn unknown_and_revoked_keys_are_rejected() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    assert_eq!(chat(&app, "authorization", "Bearer 1r_bogus").await.status(), 401);

    let created = create_key(&app, "bob").await;
    let raw = created["api_key"].as_str().unwrap();
    let id = created["id"].as_str().unwrap();
    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 200);

    let (k, v) = auth_header(&app.secret);
    let revoked: Value = reqwest::Client::new()
        .delete(format!("{}/admin/client-keys/{id}", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(revoked["revoked_at"].is_string());
    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 401);
}

#[tokio::test]
async fn open_access_logs_unknown_callers_as_anonymous_but_still_names_known_keys() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_key(&app, "carol").await;
    let raw = created["api_key"].as_str().unwrap();

    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .patch(format!("{}/admin/settings/auth-mode", app.base_url))
        .header(k, v)
        .json(&json!({ "require_shared_secret": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(chat(&app, "authorization", "Bearer whatever").await.status(), 200);
    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 200);

    let names: Vec<Option<String>> = logged_callers(&app).await.into_iter().map(|r| r.0).collect();
    assert_eq!(names, vec![None, Some("carol".into())]);
}

#[tokio::test]
async fn admin_lists_keys_without_secrets_and_reports_per_caller_stats() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_key(&app, "dave").await;
    let raw = created["api_key"].as_str().unwrap();
    chat(&app, "authorization", &format!("Bearer {raw}")).await;
    chat(&app, "authorization", &format!("Bearer {raw}")).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    let list: Value = client
        .get(format!("{}/admin/client-keys", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = list.to_string();
    assert!(!text.contains(raw), "raw key must never be listed");
    assert!(list[0].get("api_key").is_none());
    assert_eq!(list[0]["name"], "dave");
    assert!(list[0]["last_used_at"].is_string());

    let stats: Value = client
        .get(format!("{}/admin/stats/callers", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["callers"][0]["caller_name"], "dave");
    assert_eq!(stats["callers"][0]["total"], 2);
    assert_eq!(stats["callers"][0]["successes"], 2);
}

#[tokio::test]
async fn client_keys_cannot_reach_admin_api() {
    let app = spawn_app().await;
    let created = create_key(&app, "erin").await;
    let raw = created["api_key"].as_str().unwrap();
    let resp = reqwest::Client::new()
        .get(format!("{}/admin/client-keys", app.base_url))
        .header("authorization", format!("Bearer {raw}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn dataset_log_records_the_caller_as_user_id() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .patch(format!("{}/admin/providers/p1", app.base_url))
        .header(k, v)
        .json(&json!({ "dataset_logging": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let created = create_key(&app, "frank").await;
    let raw = created["api_key"].as_str().unwrap();
    let resp = chat(&app, "authorization", &format!("Bearer {raw}")).await;
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap(); // drain so the dataset tee fires
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut rd = tokio::fs::read_dir(app.dataset_log_dir.join("p1")).await.unwrap();
    let file = rd.next_entry().await.unwrap().unwrap();
    let line = tokio::fs::read_to_string(file.path()).await.unwrap();
    let record: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(record["user_id"], "frank");
}
