use anyhow::Result;
use std::sync::Arc;

use router::core::config::{self, Config, SecretSource};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{load_snapshot, AppState};
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
        // build_client needs a Config, and a Config needs a secret - which the
        // wizard may be about to create. Use a plain client for the wizard's
        // own requests (only the OAuth exchange + model probes) rather than
        // ordering the two around each other.
        let http = reqwest::Client::new();
        onboarding::run_wizard(&db, &http, &sqlite_path).await?;
        return Ok(());
    }

    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let addr =
            std::env::var("ROUTER_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let host = if let Some(port) = addr.rsplit_once(':').map(|(_, p)| p) {
            format!("127.0.0.1:{port}")
        } else {
            addr
        };
        let url = format!("http://{host}/health");
        let resp = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?
            .get(url)
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        anyhow::bail!("healthcheck returned {}", resp.status());
    }

    // Normal boot. Resolve the secret before anything can need it.
    let secret = match config::resolve_shared_secret(&sqlite_path)? {
        SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s),
        // TTY + no secret yet: bootstrap ONLY the secret here (not the full
        // provider-adding wizard) - a seed file must still be able to win
        // over interactive setup even when a secret has to be created, per
        // the design's "seed always wins" rule. Whether the provider-adding
        // loop runs at all is decided once, below, by the unified trigger
        // check (empty DB + no seed path + TTY).
        SecretSource::BootstrapNeeded if onboarding::stdin_is_tty() => {
            Some(onboarding::resolve_or_prompt_secret(&sqlite_path)?)
        }
        SecretSource::BootstrapNeeded => {
            // Headless first boot: auto-generate, persist, and log it ONCE.
            let s = config::generate_secret();
            config::persist_secret(&sqlite_path, &s)?;
            tracing::info!(
                path = ?config::secret_file_path(&sqlite_path),
                "generated a new admin shared secret sidecar. The secret value is never logged; \
                 read the sidecar file once and store it securely, or set ROUTER_SHARED_SECRET \
                 to control it explicitly."
            );
            Some(s)
        }
    };
    let secret = secret.expect("all resolve_shared_secret arms above produce a secret");

    let db = init_pool(&sqlite_path).await?;
    seed_if_configured_first(&db).await?;

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
        onboarding::run_wizard(&db, &http, &sqlite_path).await?;
    }

    let cfg = Config::from_env_with_secret(secret)?;
    let http = build_client(&cfg);
    let snapshot = load_snapshot(&db).await?;
    let log_tx = spawn_writer(db.clone(), 4096, 100);

    let state = AppState {
        db,
        http,
        config: Arc::new(cfg.clone()),
        snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
        proxy_semaphore: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_requests)),
    };

    spawn_background_refresh(state.clone());

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

    axum::serve(listener, router)
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
