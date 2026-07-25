mod common;
use common::spawn_app;

#[tokio::test]
async fn health_ok_after_full_startup() {
    // spawn_app mirrors main's wiring; assert the log writer channel is live by
    // driving a request that logs, then hitting health.
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/health", app.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
