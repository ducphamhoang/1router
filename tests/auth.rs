mod common;
use common::{auth_header, spawn_app};
use router::auth::middleware::bearer_matches;

#[test]
fn bearer_compare_requires_exact_constant_time_match() {
    assert!(bearer_matches("test-secret", "test-secret"));
    assert!(!bearer_matches("test-secret", "test-secreu"));
    assert!(!bearer_matches("test-secret", "test-secret-extra"));
}

#[tokio::test]
async fn missing_bearer_is_401() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/stats", app.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn wrong_bearer_is_401() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/stats", app.base_url))
        .header("authorization", "Bearer nope")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn correct_bearer_passes_auth() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/stats", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    // /admin/stats is a real guarded route now, so a correct bearer reaches it and
    // returns 200 (empty totals against a fresh db), proving auth let the request through.
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn admin_secret_does_not_authorize_proxy_when_set() {
    let app = common::spawn_app_with_admin_secret("admin-secret").await;
    let client = reqwest::Client::new();

    let admin_with_shared = client
        .get(format!("{}/admin/stats", app.base_url))
        .header("authorization", "Bearer test-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(admin_with_shared.status(), 401);

    let admin_with_admin = client
        .get(format!("{}/admin/stats", app.base_url))
        .header("authorization", "Bearer admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(admin_with_admin.status(), 200);

    let proxy_with_admin = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header("authorization", "Bearer admin-secret")
        .json(&serde_json::json!({ "model": "missing-pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(proxy_with_admin.status(), 401);

    let proxy_with_shared = client
        .post(format!("{}/v1/chat/completions", app.base_url))
        .header("authorization", "Bearer test-secret")
        .json(&serde_json::json!({ "model": "missing-pool", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_ne!(proxy_with_shared.status(), 401);
}
