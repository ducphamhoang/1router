use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::admin::auth::session;
use crate::core::state::AppState;

pub async fn require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let current_secret = state.shared_secret.load();
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == current_secret.as_str())
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "unauthorized" } })),
        )
            .into_response()
    }
}

pub async fn require_admin_session(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let https = session::is_https(headers);

    if let Some(raw) = session::extract_cookie(headers, https) {
        if let Ok(Some(row)) = session::validate_session(&state.db, raw).await {
            let _ = session::renew_if_needed(
                &state.db,
                &row.token_hash,
                row.created_at,
                row.expires_at,
            )
            .await;

            req.extensions_mut().insert(session::AdminSession {
                token_hash: row.token_hash,
            });
            return next.run(req).await;
        }
    }

    let current_secret = state.shared_secret.load();
    let bearer_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == current_secret.as_str())
        .unwrap_or(false);

    if bearer_ok {
        return next.run(req).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "message": "unauthorized" } })),
    )
        .into_response()
}

#[cfg(test)]
mod require_admin_session_tests {
    use super::*;
    use crate::admin::auth::session;
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("require_admin_session.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();
        std::mem::forget(dir);
        let (log_tx, _log_rx) = tokio::sync::mpsc::channel(16);

        AppState {
            db,
            http: reqwest::Client::new(),
            config: Arc::new(Config {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                sqlite_path: path.to_string_lossy().to_string(),
                shared_secret: "test-secret".to_string(),
                seed_path: None,
                connect_timeout: Duration::from_secs(30),
                ttfb_timeout: Duration::from_secs(30),
                idle_timeout: Duration::from_secs(30),
                max_body_bytes: 1024 * 1024,
                drain_timeout: Duration::from_secs(30),
            }),
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: Vec::new(),
                pools: Vec::new(),
            })),
            runtime: Arc::new(DashMap::new()),
            log_tx,
            refresh_locks: Arc::new(DashMap::new()),
            shared_secret: Arc::new(ArcSwap::from_pointee("test-secret".to_string())),
            secret_origin: SecretOrigin::SidecarFile,
            login_attempts: Arc::new(DashMap::new()),
        }
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .route("/protected", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_session,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn require_admin_session_rejects_with_neither_cookie_nor_bearer() {
        let state = state().await;
        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_admin_session_accepts_valid_bearer() {
        let state = state().await;
        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_admin_session_accepts_valid_session_cookie() {
        let state = state().await;
        let (raw, _) = session::create_session(&state.db).await.unwrap();

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, format!("admin_session={raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_admin_session_rejects_expired_session_cookie() {
        let state = state().await;
        let raw = "expired";
        let token_hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(raw.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let now = chrono::Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(token_hash)
        .bind(now - chrono::Duration::hours(2))
        .bind(now - chrono::Duration::hours(1))
        .execute(&state.db)
        .await
        .unwrap();

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, "admin_session=expired")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_admin_session_falls_back_to_bearer_when_cookie_is_garbage() {
        let state = state().await;

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, "admin_session=garbage")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }
}
