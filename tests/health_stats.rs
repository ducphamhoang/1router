use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use router::app::build_router;
use router::core::config::Config;
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{AppState, ConfigSnapshot};
use tower::ServiceExt;

pub struct TestApp {
    pub state: AppState,
}

pub fn auth_header(secret: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {secret}"))
}

async fn spawn_app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = init_pool(db_path.to_str().unwrap()).await.unwrap();
    let secret = "test-secret".to_string();
    let cfg = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path.to_string_lossy().into_owned(),
        shared_secret: secret,
        shared_secrets: vec!["test-secret".into()],
        admin_secret: None,
        seed_path: None,
        connect_timeout: Duration::from_secs(1),
        ttfb_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_body_bytes: 1024,
        max_concurrent_requests: 256,
        allow_insecure_upstreams: true,
        drain_timeout: Duration::from_secs(1),
    };
    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);
    let state = AppState {
        http: build_client(&cfg),
        config: Arc::new(cfg.clone()),
        snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
            providers: vec![],
            pools: vec![],
        })),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        proxy_semaphore: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_requests)),
        db,
    };
    std::mem::forget(dir);

    TestApp { state }
}

#[tokio::test]
async fn health_is_unauthenticated_and_ok() {
    let app = spawn_app().await;
    let router = build_router(app.state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["db"], true);
    assert_eq!(body["live_pool"], false); // no pools seeded
}

#[tokio::test]
async fn stats_totals_reflect_request_log() {
    let app = spawn_app().await;
    sqlx::query(
        "INSERT INTO request_log (pool_id, provider_id, status_code, latency_ms, success, created_at)
         VALUES ('gpt-4o','p1',200,10,1,'2026-01-01T00:00:00Z'),
                ('gpt-4o','p1',500,20,0,'2026-01-01T00:00:00Z')",
    )
    .execute(&app.state.db)
    .await
    .unwrap();

    let (k, v) = auth_header(&app.state.config.shared_secret);
    let router = build_router(app.state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/admin/stats")
                .header(k, v)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["total"], 2);
    assert_eq!(body["successes"], 1);
    assert_eq!(body["failures"], 1);
}
