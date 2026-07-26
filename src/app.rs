use axum::Router;

use crate::auth::middleware::{require_admin_bearer, require_bearer};
use crate::core::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let admin = Router::new()
        .merge(crate::telemetry::stats::routes())
        .merge(crate::providers::routes())
        .merge(crate::providers::oauth_routes::routes())
        .merge(crate::pools::routes::routes())
        .merge(crate::admin::routes())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_admin_bearer,
        ));

    let proxy = crate::proxy::routes::routes().route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        require_bearer,
    ));

    Router::new()
        .merge(crate::telemetry::health::routes())
        .merge(admin)
        .merge(proxy)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::http_client::build_client;
    use crate::core::state::{AppState, ConfigSnapshot};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        std::mem::forget(dir); // keep temp dir alive for the test process
        let db = init_pool(":memory:").await.unwrap();
        let cfg = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "s".into(),
            admin_secret: None,
            seed_path: None,
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            max_concurrent_requests: 256,
            drain_timeout: Duration::from_secs(1),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        AppState {
            http: build_client(&cfg),
            config: Arc::new(cfg.clone()),
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: vec![],
                pools: vec![],
            })),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx: tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            proxy_semaphore: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_requests)),
            db,
        }
    }

    #[tokio::test]
    async fn health_route_is_wired() {
        let app = build_router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
