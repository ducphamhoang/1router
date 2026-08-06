use axum::Router;

use crate::auth::middleware::{require_admin_session, require_bearer, require_csrf_header};
use crate::core::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let admin_authenticated = Router::new()
        .merge(crate::telemetry::stats::routes())
        .merge(crate::providers::routes())
        .merge(crate::providers::oauth_routes::routes())
        .merge(crate::pools::routes::routes())
        .merge(crate::admin::routes())
        .merge(crate::admin::auth::routes::routes())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_admin_session,
        ));

    let admin_public = crate::admin::auth::routes::public_routes()
        .layer(axum::middleware::from_fn(require_csrf_header));

    let admin = Router::new().merge(admin_authenticated).merge(admin_public);

    let proxy = crate::proxy::routes::routes().route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        require_bearer,
    ));

    let router = Router::new()
        .merge(crate::telemetry::health::routes())
        .merge(admin)
        .merge(proxy);

    #[cfg(feature = "ui")]
    let router = router.merge(crate::ui_assets::routes());

    router.with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::auth::{password, session};
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::http_client::build_client;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
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
            seed_path: None,
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        AppState {
            http: build_client(&cfg),
            shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
            config: Arc::new(cfg),
            secret_origin: SecretOrigin::SidecarFile,
            require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            auth_mode_origin: crate::core::state::AuthModeOrigin::Default,
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: vec![],
                pools: vec![],
            })),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx: tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            login_attempts: Arc::new(dashmap::DashMap::new()),
            discovered_models: Arc::new(dashmap::DashMap::new()),
            db,
        }
    }

    fn connect_addr() -> ConnectInfo<SocketAddr> {
        ConnectInfo("127.0.0.1:12345".parse().unwrap())
    }

    async fn seed_admin(db: &sqlx::SqlitePool, plain: &str) {
        let hash = password::hash_password(plain).unwrap();
        sqlx::query(
            "INSERT INTO admin_users (id, username, password_hash, updated_at)
             VALUES (1, 'admin', ?, '2026-01-01T00:00:00Z')",
        )
        .bind(hash)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn health_still_unauthenticated() {
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

    #[tokio::test]
    async fn admin_login_reachable_without_any_auth() {
        let state = test_state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/admin/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-requested-with", "1router-ui")
                    .extension(connect_addr())
                    .body(Body::from(
                        json!({"username":"admin","password":"correct-password"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_some());
    }

    #[tokio::test]
    async fn admin_providers_requires_auth_401_with_neither_cookie_nor_bearer() {
        let app = build_router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_providers_accepts_bearer_still() {
        let app = build_router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/providers")
                    .header(header::AUTHORIZATION, "Bearer s")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_providers_accepts_session_cookie() {
        let state = test_state().await;
        let (raw, _) = session::create_session(&state.db).await.unwrap();
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/providers")
                    .header(header::COOKIE, format!("admin_session={raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn v1_models_requires_bearer_only_cookie_alone_is_insufficient() {
        let state = test_state().await;
        let (raw, _) = session::create_session(&state.db).await.unwrap();
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::COOKIE, format!("admin_session={raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn csrf_blocks_post_admin_auth_login_without_header() {
        let state = test_state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/admin/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(connect_addr())
                    .body(Body::from(
                        json!({"username":"admin","password":"correct-password"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[cfg(feature = "ui")]
    #[tokio::test]
    async fn ui_route_reachable_without_auth() {
        let app = build_router(test_state().await);
        let resp = app
            .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/ui/");
    }
}
