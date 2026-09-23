mod common;
use common::{auth_header, spawn_app};
use serde_json::{json, Value};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One passthrough provider behind pool `gpt-4o`. The mock only answers when
/// the *provider's* key (`sk-test`) is sent upstream, so every successful
/// proxy call below also proves the user's own key isn't forwarded.
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

async fn create_user(app: &common::TestApp, id: &str) -> Value {
    let (k, v) = auth_header(&app.secret);
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/users", app.base_url))
        .header(k, v)
        .json(&json!({ "id": id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    resp.json().await.unwrap()
}

async fn admin_post(app: &common::TestApp, path: &str) -> reqwest::Response {
    let (k, v) = auth_header(&app.secret);
    reqwest::Client::new()
        .post(format!("{}{path}", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap()
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

async fn logged_users(app: &common::TestApp) -> Vec<Option<String>> {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let rows: Vec<(Option<String>,)> = sqlx::query_as("SELECT user_id FROM request_log ORDER BY id")
        .fetch_all(&app.db)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.0).collect()
}

#[tokio::test]
async fn user_key_authenticates_and_is_logged_by_user_id() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_user(&app, "alice").await;
    let raw = created["api_key"].as_str().unwrap();

    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 200);
    // Anthropic-style SDKs send the key as x-api-key.
    assert_eq!(chat(&app, "x-api-key", raw).await.status(), 200);

    assert_eq!(logged_users(&app).await, vec![Some("alice".into()), Some("alice".into())]);
}

#[tokio::test]
async fn shared_secret_is_logged_as_admin() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    assert_eq!(chat(&app, "authorization", &format!("Bearer {}", app.secret)).await.status(), 200);
    assert_eq!(logged_users(&app).await, vec![Some("admin".into())]);
}

#[tokio::test]
async fn unknown_and_revoked_keys_are_rejected() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    assert_eq!(chat(&app, "authorization", "Bearer 1r_bogus").await.status(), 401);

    let created = create_user(&app, "bob").await;
    let raw = created["api_key"].as_str().unwrap();
    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 200);

    let revoked: Value = admin_post(&app, "/admin/users/bob/revoke").await.json().await.unwrap();
    assert!(revoked["revoked_at"].is_string());
    assert_eq!(chat(&app, "authorization", &format!("Bearer {raw}")).await.status(), 401);
}

#[tokio::test]
async fn open_access_logs_unknown_callers_as_anonymous_but_still_attributes_known_keys() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_user(&app, "carol").await;
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

    assert_eq!(logged_users(&app).await, vec![None, Some("carol".into())]);
}

#[tokio::test]
async fn admin_lists_users_without_secrets_and_reports_per_user_stats() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_user(&app, "dave").await;
    let raw = created["api_key"].as_str().unwrap();
    chat(&app, "authorization", &format!("Bearer {raw}")).await;
    chat(&app, "authorization", &format!("Bearer {raw}")).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    let list: Value = client
        .get(format!("{}/admin/users", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!list.to_string().contains(raw), "raw key must never be listed");
    assert!(list[0].get("api_key").is_none());
    assert!(list[0].get("key_hash").is_none());
    assert_eq!(list[0]["id"], "dave");
    assert!(list[0]["last_used_at"].is_string());

    let stats: Value = client
        .get(format!("{}/admin/stats/users", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["users"][0]["user_id"], "dave");
    assert_eq!(stats["users"][0]["total"], 2);
    assert_eq!(stats["users"][0]["successes"], 2);
}

#[tokio::test]
async fn user_keys_cannot_reach_admin_api() {
    let app = spawn_app().await;
    let created = create_user(&app, "erin").await;
    let raw = created["api_key"].as_str().unwrap();
    let resp = reqwest::Client::new()
        .get(format!("{}/admin/users", app.base_url))
        .header("authorization", format!("Bearer {raw}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn dataset_log_records_the_user_id() {
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

    let created = create_user(&app, "frank").await;
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

#[tokio::test]
async fn rotate_swaps_the_key_and_keeps_attribution() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_user(&app, "gina").await;
    let old = created["api_key"].as_str().unwrap().to_string();

    let resp = admin_post(&app, "/admin/users/gina/rotate").await;
    assert_eq!(resp.status(), 200);
    let rotated: Value = resp.json().await.unwrap();
    let new = rotated["api_key"].as_str().unwrap();
    assert_ne!(new, old);
    assert_eq!(rotated["id"], "gina");

    assert_eq!(chat(&app, "authorization", &format!("Bearer {old}")).await.status(), 401);
    assert_eq!(chat(&app, "authorization", &format!("Bearer {new}")).await.status(), 200);
    assert_eq!(logged_users(&app).await, vec![Some("gina".into())]);

    admin_post(&app, "/admin/users/gina/revoke").await;
    assert_eq!(admin_post(&app, "/admin/users/gina/rotate").await.status(), 409);
    assert_eq!(admin_post(&app, "/admin/users/nobody/rotate").await.status(), 404);
}

#[tokio::test]
async fn create_rejects_duplicate_and_reserved_ids() {
    let app = spawn_app().await;
    create_user(&app, "hank").await;
    let (k, v) = auth_header(&app.secret);
    let client = reqwest::Client::new();
    for (id, status) in [("hank", 409), ("admin", 400), ("a/b", 400)] {
        let resp = client
            .post(format!("{}/admin/users", app.base_url))
            .header(&k, &v)
            .json(&json!({ "id": id }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), status, "id {id:?}");
    }
}

#[tokio::test]
async fn export_import_round_trip_keeps_user_keys_working() {
    let src = spawn_app().await;
    let created = create_user(&src, "ivy").await;
    let raw = created["api_key"].as_str().unwrap().to_string();
    create_user(&src, "jay").await;
    admin_post(&src, "/admin/users/jay/revoke").await;

    let (k, v) = auth_header(&src.secret);
    let client = reqwest::Client::new();
    let dump: Value = client
        .get(format!("{}/admin/export", src.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!dump.to_string().contains(&raw), "export must never carry the raw key");
    assert_eq!(dump["users"].as_array().unwrap().len(), 2);

    // Restore into a fresh instance: ivy's original raw key must work there,
    // and jay must stay revoked.
    let dst = spawn_app().await;
    let _upstream = setup(&dst).await;
    let (k, v) = auth_header(&dst.secret);
    let resp = client
        .post(format!("{}/admin/import", dst.base_url))
        .header(&k, &v)
        .json(&dump)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(chat(&dst, "authorization", &format!("Bearer {raw}")).await.status(), 200);
    assert_eq!(logged_users(&dst).await, vec![Some("ivy".into())]);

    let users: Value = client
        .get(format!("{}/admin/users", dst.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let jay = users.as_array().unwrap().iter().find(|u| u["id"] == "jay").unwrap();
    assert!(jay["revoked_at"].is_string());
}

/// SEC-06: a user key may direct-address only models the admin exposed
/// (provider default, a pool member's model, or the discovered list);
/// the shared secret stays unrestricted.
#[tokio::test]
async fn user_keys_can_only_direct_address_enabled_models() {
    let app = spawn_app().await;
    let _upstream = setup(&app).await;
    let created = create_user(&app, "kim").await;
    let user = format!("Bearer {}", created["api_key"].as_str().unwrap());
    let admin = format!("Bearer {}", app.secret);

    let direct = |auth: String, model: &'static str| {
        let url = format!("{}/v1/chat/completions", app.base_url);
        async move {
            reqwest::Client::new()
                .post(url)
                .header("authorization", auth)
                .json(&json!({ "model": model, "messages": [] }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };

    assert_eq!(direct(user.clone(), "p1/real-model").await, 200, "provider default is enabled");
    assert_eq!(direct(user.clone(), "p1/most-expensive").await, 400, "arbitrary model is not");
    assert_eq!(direct(admin, "p1/most-expensive").await, 200, "admin is unrestricted");
}
