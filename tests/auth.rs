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
