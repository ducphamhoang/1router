use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use router::app::build_router;
use router::core::config::Config;
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::model::WireFormat;
use router::core::state::{AppState, ConfigSnapshot};
use serde_json::json;
use tower::ServiceExt;

async fn test_state() -> AppState {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = init_pool(db_path.to_str().unwrap()).await.unwrap();
    let cfg = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path.to_string_lossy().into_owned(),
        shared_secret: "test-secret".into(),
        seed_path: None,
        connect_timeout: Duration::from_secs(1),
        ttfb_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_body_bytes: 1024,
        drain_timeout: Duration::from_secs(1),
    };
    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);
    let state = AppState {
        http: build_client(&cfg),
        config: Arc::new(cfg),
        snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
            providers: vec![],
            pools: vec![],
        })),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        db,
    };
    std::mem::forget(dir);
    state
}

async fn create_provider(db: &sqlx::SqlitePool, id: &str, wire: WireFormat) {
    sqlx::query(
        "INSERT INTO providers
            (id, name, wire_format, kind, base_url, api_key, upstream_model, created_at, updated_at)
         VALUES (?, ?, ?, 'passthrough', 'https://x', 'k', 'm', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(id)
    .bind(id)
    .bind(wire)
    .execute(db)
    .await
    .unwrap();
}

fn auth_header(secret: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {secret}"))
}

fn json_request(method: Method, uri: &str, secret: &str, body: serde_json::Value) -> Request<Body> {
    let (k, v) = auth_header(secret);
    Request::builder()
        .method(method)
        .uri(uri)
        .header(k, v)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn empty_request(method: Method, uri: &str, secret: &str) -> Request<Body> {
    let (k, v) = auth_header(secret);
    Request::builder()
        .method(method)
        .uri(uri)
        .header(k, v)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn create_pool_add_member() {
    let state = test_state().await;
    create_provider(&state.db, "p1", WireFormat::OpenAi).await;
    let router = build_router(state.clone());

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            &state.config.shared_secret,
            json!({ "id": "gpt-4o", "wire_format": "openai" }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);

    let m = router
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/gpt-4o/members",
            &state.config.shared_secret,
            json!({ "provider_id": "p1", "priority": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(m.status(), StatusCode::OK);

    let list = router
        .oneshot(empty_request(
            Method::GET,
            "/admin/pools/gpt-4o/members",
            &state.config.shared_secret,
        ))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let bytes = to_bytes(list.into_body(), usize::MAX).await.unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn wire_format_mismatch_is_400() {
    let state = test_state().await;
    create_provider(&state.db, "anth", WireFormat::Anthropic).await;
    let router = build_router(state.clone());

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            &state.config.shared_secret,
            json!({ "id": "gpt-4o", "wire_format": "openai" }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);

    let m = router
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/gpt-4o/members",
            &state.config.shared_secret,
            json!({ "provider_id": "anth", "priority": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(m.status(), StatusCode::BAD_REQUEST);
}
