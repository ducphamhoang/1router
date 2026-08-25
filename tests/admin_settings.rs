use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use router::app::build_router;
use router::core::config::{secret_file_path, Config};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{AppState, AuthModeOrigin, ConfigSnapshot, SecretOrigin};
use serde_json::json;
use tower::ServiceExt;

async fn test_state(secret_origin: SecretOrigin) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = init_pool(db_path.to_str().unwrap()).await.unwrap();
    let cfg = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path.to_string_lossy().into_owned(),
        shared_secret: "initial".into(),
        seed_path: None,
        connect_timeout: Duration::from_secs(1),
        ttfb_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_body_bytes: 1024,
        drain_timeout: Duration::from_secs(1),
    };
    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);
    let state = AppState {
        db,
        http: build_client(&cfg),
        config: Arc::new(cfg.clone()),
        shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
        secret_origin,
        require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        auth_mode_origin: AuthModeOrigin::Default,
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
    };
    (state, dir)
}

fn request(
    method: Method,
    uri: &str,
    secret: &str,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"));

    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };

    builder.body(body).unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn shared_secret_get_masks_by_default_and_reveals_explicitly() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let router = build_router(state);

    let masked = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(masked.status(), StatusCode::OK);
    let body = json_body(masked).await;
    assert_eq!(body["shared_secret"], "***tial");
    assert_eq!(body["masked"], true);
    assert_eq!(body["origin"], "sidecar_file");

    let revealed = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(revealed.status(), StatusCode::OK);
    let body = json_body(revealed).await;
    assert_eq!(body["shared_secret"], "initial");
    assert_eq!(body["masked"], false);
    assert_eq!(body["origin"], "sidecar_file");
}

#[tokio::test]
async fn shared_secret_patch_persists_and_rotates_live_bearer_secret() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let secret_path = secret_file_path(&state.config.sqlite_path);
    let router = build_router(state);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/shared-secret",
            "initial",
            Some(json!({ "shared_secret": "rotated-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let body = json_body(patched).await;
    assert_eq!(body["shared_secret"], "***cret");
    assert_eq!(body["masked"], true);
    assert_eq!(body["origin"], "sidecar_file");
    assert_eq!(
        std::fs::read_to_string(secret_path).unwrap(),
        "rotated-secret"
    );

    let old_secret = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(old_secret.status(), StatusCode::UNAUTHORIZED);

    let new_secret = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "rotated-secret",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(new_secret.status(), StatusCode::OK);
    let body = json_body(new_secret).await;
    assert_eq!(body["shared_secret"], "rotated-secret");
}

#[tokio::test]
async fn security_status_flags_the_default_secret_and_clears_once_rotated() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    // test_state's "initial" isn't the published default - swap it in
    // directly so this test exercises the real default value rather than
    // duplicating it as a literal.
    state.shared_secret.store(Arc::new(
        router::core::config::DEFAULT_SHARED_SECRET.to_string(),
    ));
    let router = build_router(state);

    let default_status = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/security-status",
            router::core::config::DEFAULT_SHARED_SECRET,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(default_status.status(), StatusCode::OK);
    let body = json_body(default_status).await;
    assert_eq!(body["shared_secret_is_default"], true);
    // No admin_users row in this test's fresh DB, so nothing to flag as a
    // default password - that path is covered at the onboarding-module
    // level (admin_password_is_default_is_false_once_rotated).
    assert_eq!(body["admin_password_is_default"], false);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/shared-secret",
            router::core::config::DEFAULT_SHARED_SECRET,
            Some(json!({ "shared_secret": "a-real-rotated-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);

    let rotated_status = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/security-status",
            "a-real-rotated-secret",
            None,
        ))
        .await
        .unwrap();
    let body = json_body(rotated_status).await;
    assert_eq!(body["shared_secret_is_default"], false);
}

#[tokio::test]
async fn shared_secret_patch_conflicts_when_secret_origin_is_env() {
    let (state, _dir) = test_state(SecretOrigin::Env).await;
    let secret_path = secret_file_path(&state.config.sqlite_path);
    let router = build_router(state);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/shared-secret",
            "initial",
            Some(json!({ "shared_secret": "rotated-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::CONFLICT);
    let body = json_body(patched).await;
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("ROUTER_SHARED_SECRET"));
    assert!(!secret_path.exists());

    let old_secret_still_works = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(old_secret_still_works.status(), StatusCode::OK);

    let new_secret_does_not_work = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "rotated-secret",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(new_secret_does_not_work.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_mode_get_reflects_current_state_and_origin() {
    let (mut state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    state
        .require_shared_secret
        .store(false, std::sync::atomic::Ordering::Relaxed);
    state.auth_mode_origin = AuthModeOrigin::Db;
    let router = build_router(state);

    let response = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/auth-mode",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body,
        json!({"require_shared_secret": false, "origin": "db"})
    );
}

#[tokio::test]
async fn auth_mode_patch_toggles_and_takes_effect_on_next_request_without_restart() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let router = build_router(state);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/auth-mode",
            "initial",
            Some(json!({"require_shared_secret": false})),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    assert_eq!(json_body(patched).await["require_shared_secret"], false);

    let open_request = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(open_request.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_mode_patch_conflicts_when_env_controlled() {
    let (mut state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    state.auth_mode_origin = AuthModeOrigin::Env;
    let router = build_router(state);

    let response = router
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/auth-mode",
            "initial",
            Some(json!({"require_shared_secret": false})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(json_body(response).await["error"]["message"]
        .as_str()
        .unwrap()
        .contains("ROUTER_REQUIRE_SHARED_SECRET"));
}

#[tokio::test]
async fn security_status_reports_require_shared_secret_and_loopback() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let router = build_router(state);

    let response = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/security-status",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["require_shared_secret"], true);
    assert_eq!(body["listen_addr_is_loopback"], true);
}
