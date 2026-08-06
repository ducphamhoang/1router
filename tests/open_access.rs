use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::Request;
use router::app::build_router;
use router::core::config::{self, Config, SecretSource};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{AppState, AuthModeOrigin, ConfigSnapshot, SecretOrigin};
use tower::ServiceExt;

async fn state_for(source: &SecretSource) -> AppState {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("open-access.db");
    let db = init_pool(db_path.to_str().unwrap()).await.unwrap();
    std::mem::forget(dir);
    let cfg = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path.to_string_lossy().into_owned(),
        shared_secret: "existing-secret".into(),
        seed_path: None,
        connect_timeout: Duration::from_secs(1),
        ttfb_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_body_bytes: 1024,
        drain_timeout: Duration::from_secs(1),
    };
    let mode = config::resolve_auth_mode(source, None).unwrap();
    let (require, origin) = match mode {
        config::AuthModeSource::Env(value) => (value, AuthModeOrigin::Env),
        config::AuthModeSource::Db(value) => (value, AuthModeOrigin::Db),
        config::AuthModeSource::Default(value) => (value, AuthModeOrigin::Default),
    };
    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);
    AppState {
        db,
        http: build_client(&cfg),
        config: Arc::new(cfg.clone()),
        shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret)),
        secret_origin: SecretOrigin::SidecarFile,
        require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(require)),
        auth_mode_origin: origin,
        snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
            providers: vec![],
            pools: vec![],
        })),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        login_attempts: Arc::new(dashmap::DashMap::new()),
        discovered_models: Arc::new(dashmap::DashMap::new()),
    }
}

#[tokio::test]
async fn upgrade_with_existing_secret_still_requires_bearer() {
    let app = build_router(state_for(&SecretSource::SidecarFile("existing-secret".into())).await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/chat/completions")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn fresh_bootstrap_defaults_to_open_access() {
    let app = build_router(state_for(&SecretSource::BootstrapNeeded).await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}
