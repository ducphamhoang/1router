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
use router::core::state::{AppState, ConfigSnapshot, SecretOrigin};
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
        shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
        config: Arc::new(cfg),
        secret_origin: SecretOrigin::SidecarFile,
        require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        auth_mode_origin: router::core::state::AuthModeOrigin::Default,
        snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
            providers: vec![],
            pools: vec![],
        })),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        login_attempts: Arc::new(dashmap::DashMap::new()),
        discovered_models: Arc::new(dashmap::DashMap::new()),
        pool_rotation: Arc::new(dashmap::DashMap::new()),
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

async fn create_codex_provider(db: &sqlx::SqlitePool, id: &str, wire: WireFormat) {
    sqlx::query(
        "INSERT INTO providers
            (id, name, wire_format, kind, base_url, api_key, upstream_model, created_at, updated_at)
         VALUES (?, ?, ?, 'oauth_codex', NULL, NULL, 'gpt-5-codex', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
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
    let secret = state.shared_secret.load();

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
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
            secret.as_str(),
            json!({ "provider_id": "p1", "priority": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(m.status(), StatusCode::OK);

    let list = router
        .oneshot(empty_request(
            Method::GET,
            "/admin/pools/gpt-4o/members",
            secret.as_str(),
        ))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let bytes = to_bytes(list.into_body(), usize::MAX).await.unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn passthrough_provider_can_join_a_pool_with_a_different_stored_wire_format() {
    // `PassthroughAdapter` now translates between wire formats (see the
    // universal passthrough translation design doc), so a provider whose
    // own wire_format differs from the pool's is no longer rejected - it
    // behaves the same as the Codex/Command Code adapters already did.
    let state = test_state().await;
    create_provider(&state.db, "anth", WireFormat::Anthropic).await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({ "id": "gpt-4o", "wire_format": "openai" }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);

    let m = router
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/gpt-4o/members",
            secret.as_str(),
            json!({ "provider_id": "anth", "priority": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(m.status(), StatusCode::OK);
}

#[tokio::test]
async fn codex_provider_can_join_a_pool_with_a_different_stored_wire_format() {
    let state = test_state().await;
    create_codex_provider(&state.db, "cx", WireFormat::OpenAi).await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({ "id": "claude", "wire_format": "anthropic" }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);

    let m = router
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/claude/members",
            secret.as_str(),
            json!({ "provider_id": "cx", "priority": 10 }),
        ))
        .await
        .unwrap();
    assert_eq!(m.status(), StatusCode::OK);
}

#[tokio::test]
async fn create_pool_defaults_to_priority_strategy() {
    let state = test_state().await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    let c = router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({ "id": "gpt-4o", "wire_format": "openai" }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);
    let bytes = to_bytes(c.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["strategy"], "priority");
    assert_eq!(body["sticky_limit"], serde_json::Value::Null);
}

#[tokio::test]
async fn create_pool_accepts_round_robin_strategy() {
    let state = test_state().await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    let c = router
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({
                "id": "gpt-4o", "wire_format": "openai",
                "strategy": "round_robin", "sticky_limit": 3
            }),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::CREATED);
    let bytes = to_bytes(c.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["strategy"], "round_robin");
    assert_eq!(body["sticky_limit"], 3);
}

#[tokio::test]
async fn put_pool_updates_strategy() {
    let state = test_state().await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({ "id": "gpt-4o", "wire_format": "openai" }),
        ))
        .await
        .unwrap();

    let u = router
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/gpt-4o",
            secret.as_str(),
            json!({ "strategy": "round_robin", "sticky_limit": 2 }),
        ))
        .await
        .unwrap();
    assert_eq!(u.status(), StatusCode::OK);
    let bytes = to_bytes(u.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["strategy"], "round_robin");
    assert_eq!(body["sticky_limit"], 2);

    // Persisted, not just returned in the response.
    let list = router
        .oneshot(empty_request(Method::GET, "/admin/pools", secret.as_str()))
        .await
        .unwrap();
    let bytes = to_bytes(list.into_body(), usize::MAX).await.unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(arr[0]["strategy"], "round_robin");
}

#[tokio::test]
async fn put_pool_patching_sticky_limit_alone_does_not_reset_strategy() {
    let state = test_state().await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    router
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/admin/pools",
            secret.as_str(),
            json!({ "id": "gpt-4o", "wire_format": "openai", "strategy": "round_robin" }),
        ))
        .await
        .unwrap();

    // Patch only sticky_limit - strategy must stay round_robin, not reset
    // to the default priority.
    let u = router
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/gpt-4o",
            secret.as_str(),
            json!({ "sticky_limit": 5 }),
        ))
        .await
        .unwrap();
    assert_eq!(u.status(), StatusCode::OK);
    let bytes = to_bytes(u.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["strategy"], "round_robin");
    assert_eq!(body["sticky_limit"], 5);
}

#[tokio::test]
async fn put_pool_rejects_unknown_id() {
    let state = test_state().await;
    let router = build_router(state.clone());
    let secret = state.shared_secret.load();

    let u = router
        .oneshot(json_request(
            Method::PUT,
            "/admin/pools/nope",
            secret.as_str(),
            json!({ "strategy": "round_robin" }),
        ))
        .await
        .unwrap();
    assert_eq!(u.status(), StatusCode::NOT_FOUND);
}
