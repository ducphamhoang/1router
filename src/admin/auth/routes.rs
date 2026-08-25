use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Instant;

use crate::admin::auth::{password, rate_limit, session};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/admin/auth/login", post(login))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/auth/logout", post(logout))
        .route("/admin/auth/password", patch(change_password))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = addr.ip();
    let now = Instant::now();

    if rate_limit::is_locked_out(&state.login_attempts, ip, now) {
        tracing::warn!(%ip, "admin login blocked: rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"too many failed attempts, try again later"}})),
        )
            .into_response();
    }

    let row: Option<(String, String)> =
        match sqlx::query_as("SELECT username, password_hash FROM admin_users WHERE id = 1")
            .fetch_optional(&state.db)
            .await
        {
            Ok(row) => row,
            Err(e) => return AppError::from(e).into_response(),
        };

    let ok = row
        .as_ref()
        .map(|(username, hash)| {
            username == &req.username && password::verify_password(hash, &req.password)
        })
        .unwrap_or(false);

    if !ok {
        rate_limit::record_failure(&state.login_attempts, ip, now);
        tracing::warn!(username = %req.username, %ip, "admin login failed");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"invalid username or password"}})),
        )
            .into_response();
    }

    rate_limit::record_success(&state.login_attempts, ip);

    let (raw_token, expires_at) = match session::create_session(&state.db).await {
        Ok(session) => session,
        Err(e) => return e.into_response(),
    };

    let https = session::is_https(&headers);
    let cookie = session::build_set_cookie(&raw_token, expires_at, https);

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"ok": true})),
    )
        .into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = session::delete_all_sessions(&state.db).await {
        return e.into_response();
    }

    let https = session::is_https(&headers);
    (
        StatusCode::OK,
        [(header::SET_COOKIE, session::build_clear_cookie(https))],
        Json(json!({"ok": true})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    sess: Option<Extension<session::AdminSession>>,
    Json(req): Json<PasswordChangeRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT username, password_hash FROM admin_users WHERE id = 1")
            .fetch_optional(&state.db)
            .await?;

    let (_username, hash) =
        row.ok_or_else(|| AppError::Internal("admin_users row missing".into()))?;

    if !password::verify_password(&hash, &req.current_password) {
        return Err(AppError::Unauthorized);
    }

    if req.new_password.trim().is_empty() {
        return Err(AppError::BadRequest("new_password cannot be empty".into()));
    }

    let new_hash = password::hash_password(&req.new_password)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    sqlx::query("UPDATE admin_users SET password_hash = ?, updated_at = ? WHERE id = 1")
        .bind(&new_hash)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&state.db)
        .await?;

    if let Some(Extension(sess)) = sess {
        session::delete_all_sessions_except(&state.db, &sess.token_hash).await?;
    } else {
        session::delete_all_sessions(&state.db).await?;
    }

    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::auth::{password, session};
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::middleware;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth_routes.db");
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
            require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            auth_mode_origin: crate::core::state::AuthModeOrigin::Default,
            login_attempts: Arc::new(DashMap::new()),
            discovered_models: Arc::new(DashMap::new()),
            pool_rotation: Arc::new(DashMap::new()),
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
    async fn login_succeeds_with_correct_credentials_and_sets_cookie() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
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

        assert_eq!(res.status(), StatusCode::OK);
        let set_cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("admin_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn login_rejects_wrong_password() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(connect_addr())
                    .body(Body::from(
                        json!({"username":"admin","password":"wrong"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_locks_out_after_five_failures() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);

        let mut last = StatusCode::OK;
        for _ in 0..6 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/admin/auth/login")
                        .header(header::CONTENT_TYPE, "application/json")
                        .extension(connect_addr())
                        .body(Body::from(
                            json!({"username":"admin","password":"wrong"}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            last = res.status();
        }

        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn logout_deletes_all_sessions_not_just_current() {
        let state = state().await;
        let (raw_a, _) = session::create_session(&state.db).await.unwrap();
        let (_raw_b, _) = session::create_session(&state.db).await.unwrap();

        let app = routes().with_state(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/logout")
                    .header(header::COOKIE, format!("admin_session={raw_a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn password_change_requires_current_password() {
        let state = state().await;
        seed_admin(&state.db, "old-password").await;

        let app = routes()
            .route_layer(middleware::from_fn(
                |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(session::AdminSession {
                        token_hash: "current".to_string(),
                    });
                    next.run(req).await
                },
            ))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/admin/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"current_password":"wrong","new_password":"new-password"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(password::verify_password(&stored, "old-password"));
    }

    #[tokio::test]
    async fn password_change_invalidates_other_sessions_but_keeps_current() {
        let state = state().await;
        seed_admin(&state.db, "old-password").await;
        let (raw_a, _) = session::create_session(&state.db).await.unwrap();
        let (raw_b, _) = session::create_session(&state.db).await.unwrap();
        let current = session::validate_session(&state.db, &raw_a)
            .await
            .unwrap()
            .unwrap();
        let other = session::validate_session(&state.db, &raw_b)
            .await
            .unwrap()
            .unwrap();

        let current_hash = current.token_hash.clone();
        let other_hash = other.token_hash.clone();

        let app = routes()
            .route_layer(middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let current_hash = current_hash.clone();
                    async move {
                        req.extensions_mut().insert(session::AdminSession {
                            token_hash: current_hash,
                        });
                        next.run(req).await
                    }
                },
            ))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/admin/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"current_password":"old-password","new_password":"new-password"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let kept: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions WHERE token_hash = ?")
                .bind(&current.token_hash)
                .fetch_optional(&state.db)
                .await
                .unwrap();
        let removed: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions WHERE token_hash = ?")
                .bind(&other_hash)
                .fetch_optional(&state.db)
                .await
                .unwrap();

        assert_eq!(kept, Some(current.token_hash));
        assert!(removed.is_none());

        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(password::verify_password(&stored, "new-password"));
    }

    #[tokio::test]
    async fn password_change_with_bearer_auth_deletes_all_sessions() {
        let state = state().await;
        seed_admin(&state.db, "old-password").await;
        let (_raw_a, _) = session::create_session(&state.db).await.unwrap();
        let (_raw_b, _) = session::create_session(&state.db).await.unwrap();

        let app = routes()
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::auth::middleware::require_admin_session,
            ))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/admin/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::from(
                        json!({"current_password":"old-password","new_password":"new-password"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let session_count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(session_count, 0);

        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(password::verify_password(&stored, "new-password"));
    }
}
