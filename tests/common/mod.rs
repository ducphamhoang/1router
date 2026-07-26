use sqlx::SqlitePool;

pub struct TestApp {
    pub base_url: String,
    pub secret: String,
    pub db: SqlitePool,
}

pub async fn spawn_app() -> TestApp {
    spawn_app_with_sqlite_path(None).await
}

// Lets a caller point at a persistent SQLite file instead of a throwaway temp
// one - used by the Codex real-account e2e test so a completed OAuth login
// (and its refresh token) survives across separate `cargo test` invocations,
// instead of requiring a fresh browser login every single run.
pub async fn spawn_app_with_sqlite_path(sqlite_path: Option<String>) -> TestApp {
    // Build Config directly rather than through Config::from_env() + std::env::set_var.
    // Integration tests within one file run concurrently by default; std::env is
    // process-global, so concurrent spawn_app() calls setting ROUTER_* would race
    // each other exactly like the Task P0-5 config-test bug (see that task's fix
    // note) - constructing the struct directly removes the shared mutable state
    // instead of just serializing access to it.
    let secret = "test-secret".to_string();
    let db_path = match sqlite_path {
        Some(p) => p,
        None => {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            let p = db_file.path().to_str().unwrap().to_string();
            // leak the temp file so it lives for the whole test
            std::mem::forget(db_file);
            p
        }
    };

    let cfg = router::core::config::Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path,
        shared_secret: secret.clone(),
        seed_path: None,
        connect_timeout: std::time::Duration::from_secs(10),
        ttfb_timeout: std::time::Duration::from_secs(60),
        idle_timeout: std::time::Duration::from_secs(120),
        max_body_bytes: 10 * 1024 * 1024,
        drain_timeout: std::time::Duration::from_secs(30),
    };
    let db = router::core::db::init_pool(&cfg.sqlite_path).await.unwrap();
    let http = router::core::http_client::build_client(&cfg);
    let snapshot = router::core::state::load_snapshot(&db).await.unwrap();
    let log_tx = router::telemetry::request_log::spawn_writer(db.clone(), 1024, 50);

    let state = router::core::state::AppState {
        db: db.clone(),
        http,
        shared_secret: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            cfg.shared_secret.clone(),
        )),
        config: std::sync::Arc::new(cfg.clone()),
        secret_origin: router::core::state::SecretOrigin::SidecarFile,
        snapshot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        runtime: std::sync::Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: std::sync::Arc::new(dashmap::DashMap::new()),
        login_attempts: std::sync::Arc::new(dashmap::DashMap::new()),
    };

    let router = router::app::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    TestApp {
        base_url: format!("http://{addr}"),
        secret,
        db,
    }
}

pub fn auth_header(secret: &str) -> (String, String) {
    ("authorization".to_string(), format!("Bearer {secret}"))
}
