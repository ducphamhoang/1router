use anyhow::Result;
use std::sync::Arc;

use router::core::config::{self, Config, SecretSource};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{
    ensure_direct_pools_for_unassigned_providers, load_snapshot, AppState, AuthModeOrigin,
    SecretOrigin,
};
use router::onboarding;
use router::providers::refresh_task::spawn_background_refresh;
use router::seed::seed_if_configured;
use router::telemetry::logging::init_tracing;
use router::telemetry::request_log::spawn_writer;

/// seed_if_configured needs a Config (for `seed_path`), but the secret may
/// not exist yet at this point in boot. The seed only ever reads
/// `cfg.seed_path`, so build a throwaway Config with a dummy secret for it.
async fn seed_if_configured_first(db: &sqlx::SqlitePool) -> Result<()> {
    let cfg = Config::from_env_with_secret(String::new())?;
    seed_if_configured(db, &cfg).await
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let sqlite_path = config::sqlite_path_from_env();

    // Subcommand check first, before any other startup work. One deliberate
    // arg check instead of a CLI parser dependency (spec non-goal).
    if std::env::args().nth(1).as_deref() == Some("setup") {
        if !onboarding::stdin_is_tty() {
            eprintln!(
                "`1router setup` is interactive and needs a terminal on stdin. \
                 For scripted config, set ROUTER_SEED_PATH to a config JSON file instead."
            );
            std::process::exit(2);
        }
        let db = init_pool(&sqlite_path).await?;
        // Resolve and persist the auth-mode default before the manual menu
        // creates the sidecar secret; otherwise a fresh setup would be
        // misclassified as an upgrade and start in required mode.
        let initial_secret_source = config::resolve_shared_secret(&sqlite_path)?;
        let initial_auth_mode = config::resolve_auth_mode(
            &initial_secret_source,
            router::core::settings::get_bool(&db, "require_shared_secret").await?,
        )?;
        if let router::core::config::AuthModeSource::Default(value) = initial_auth_mode {
            router::core::settings::set_bool(&db, "require_shared_secret", value).await?;
        }

        // `--reset-admin-password` is a standalone recovery path, not part of
        // the provider/pool wizard: someone who forgot the admin UI password
        // but can still run the CLI already has filesystem access to the DB,
        // so this intentionally doesn't ask for the old password.
        if std::env::args().nth(2).as_deref() == Some("--reset-admin-password") {
            onboarding::reset_admin_password(&db).await?;
            return Ok(());
        }

        // build_client needs a Config, and a Config needs a secret - which the
        // wizard may be about to create. Use a plain client for the wizard's
        // own requests (only the OAuth exchange + model probes) rather than
        // ordering the two around each other.
        let http = reqwest::Client::new();
        onboarding::run_menu(&db, &http, &sqlite_path).await?;
        return Ok(());
    }

    // Normal boot. Resolve the secret before anything can need it.
    let resolved_secret = config::resolve_shared_secret(&sqlite_path)?;
    let mut secret_origin = SecretOrigin::from_source(&resolved_secret);

    let secret = match &resolved_secret {
        SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s.clone()),
        // TTY + no secret yet: bootstrap ONLY the secret here (not the full
        // provider-adding wizard) - a seed file must still be able to win
        // over interactive setup even when a secret has to be created, per
        // the design's "seed always wins" rule. Whether the provider-adding
        // loop runs at all is decided once, below, by the unified trigger
        // check (empty DB + no seed path + TTY).
        SecretSource::BootstrapNeeded if onboarding::stdin_is_tty() => {
            let s = onboarding::resolve_or_prompt_secret(&sqlite_path)?;
            secret_origin = Some(SecretOrigin::SidecarFile);
            Some(s)
        }
        SecretSource::BootstrapNeeded => {
            // Headless first boot: auto-generate, persist, and log it ONCE.
            let s = config::generate_secret();
            config::persist_secret(&sqlite_path, &s)?;
            tracing::info!(
                secret = %s,
                path = ?config::secret_file_path(&sqlite_path),
                "generated a new admin shared secret - SAVE THIS NOW, it will not be logged \
                 again. Set ROUTER_SHARED_SECRET to control it explicitly."
            );
            secret_origin = Some(SecretOrigin::SidecarFile);
            Some(s)
        }
    };
    let secret = secret.expect("all resolve_shared_secret arms above produce a secret");
    let secret_origin = secret_origin.expect("all resolved runtime secrets have an origin");

    let db = init_pool(&sqlite_path).await?;
    let auth_mode_source = config::resolve_auth_mode(
        &resolved_secret,
        router::core::settings::get_bool(&db, "require_shared_secret").await?,
    )?;
    if let router::core::config::AuthModeSource::Default(value) = auth_mode_source {
        router::core::settings::set_bool(&db, "require_shared_secret", value).await?;
    }
    let (require_shared_secret, auth_mode_origin) = match auth_mode_source {
        router::core::config::AuthModeSource::Env(value) => (value, AuthModeOrigin::Env),
        router::core::config::AuthModeSource::Db(value) => (value, AuthModeOrigin::Db),
        router::core::config::AuthModeSource::Default(value) => (value, AuthModeOrigin::Default),
    };

    let cfg = Config::from_env_with_secret(secret.clone())?;
    seed_if_configured_first(&db).await?;
    onboarding::resolve_or_prompt_admin_password(&db).await?;

    // Every boot, not just first boot: an operator could still be running on
    // the fast-path defaults from an earlier `1router setup` / first-boot
    // run. Both are public information (README.md), so this is a nudge, not
    // a secret-leak concern.
    if secret == config::DEFAULT_SHARED_SECRET {
        tracing::warn!(
            "the shared secret is still the published default ('{}') - change it via \
             `PATCH /admin/settings/shared-secret`, the admin UI Settings page, or \
             `1router setup`, before exposing this instance beyond localhost.",
            config::DEFAULT_SHARED_SECRET
        );
    }
    if onboarding::admin_password_is_default(&db).await? {
        tracing::warn!(
            "the admin UI password is still the published default ('{}', username: admin) - \
             change it via `1router setup --reset-admin-password` or the admin UI Settings page.",
            config::DEFAULT_ADMIN_PASSWORD
        );
    }
    if !require_shared_secret {
        if config::listen_addr_is_loopback(&cfg.listen_addr) {
            tracing::info!("open access is ON: /v1/* accepts requests with no API key");
        } else {
            tracing::warn!(
                "open access is ON: /v1/* accepts requests with no API key, and this gateway is listening on {} — reachable from other machines. Anyone who can reach it can spend your provider credits. Set ROUTER_LISTEN_ADDR=127.0.0.1:8080, or ROUTER_REQUIRE_SHARED_SECRET=true.",
                cfg.listen_addr
            );
        }
    }

    // First-boot wizard (provider/pool prompts): empty DB + no seed file + a
    // real terminal. A seed file always wins over interactive setup, even if
    // the secret itself had to be bootstrapped just above - that's a
    // separate concern from whether to prompt for providers. Any of the
    // three conditions missing means "don't block a headless/scripted
    // deployment or override a seed file's config."
    let seed_configured = std::env::var("ROUTER_SEED_PATH").is_ok();
    if !seed_configured
        && onboarding::stdin_is_tty()
        && onboarding::providers_table_is_empty(&db).await?
    {
        let http = reqwest::Client::new();
        onboarding::run_first_boot_wizard(&db, &http, &sqlite_path).await?;
    }

    ensure_direct_pools_for_unassigned_providers(&db).await?;

    let http = build_client(&cfg);
    let snapshot = load_snapshot(&db).await?;
    let log_tx = spawn_writer(db.clone(), 4096, 100);

    let state = AppState {
        db,
        http,
        config: Arc::new(cfg.clone()),
        shared_secret: Arc::new(arc_swap::ArcSwap::from_pointee(cfg.shared_secret.clone())),
        secret_origin,
        require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(require_shared_secret)),
        auth_mode_origin,
        snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        login_attempts: Arc::new(dashmap::DashMap::new()),
        discovered_models: Arc::new(dashmap::DashMap::new()),
    };

    if let Err(e) = router::admin::auth::session::delete_expired(&state.db).await {
        tracing::warn!(error = %e, "boot-time admin session sweep failed");
    }
    spawn_background_refresh(state.clone());
    router::admin::auth::cleanup::spawn_session_cleanup(state.clone());
    router::providers::routes::warm_discovered_models_cache(&state);

    let router = router::app::build_router(state);
    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    tracing::info!(addr = %cfg.listen_addr, "1router listening");

    // Fixed vs. the plan's original brief: that version slept for the full
    // drain_timeout *inside* the shutdown future before returning, which
    // delayed axum from refusing new connections until the entire drain
    // window had already elapsed, and then waited UNBOUNDED for in-flight
    // requests - the opposite of "stop accepting new work immediately, then
    // bound how long you wait for what's already in flight." Correct version:
    // the shutdown future resolves the instant a signal arrives (so axum
    // stops accepting new connections right away); a separate watchdog forces
    // the process to exit if graceful drain hasn't finished within
    // drain_timeout, so a stuck client can't hang shutdown forever.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let drain = cfg.drain_timeout;
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received, draining (up to {:?})", drain);
        let _ = shutdown_tx.send(());
        tokio::time::sleep(drain).await;
        tracing::warn!("drain timeout exceeded, forcing process exit");
        std::process::exit(1);
    });

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    })
    .await?;
    Ok(())
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
