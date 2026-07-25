use anyhow::Result;
use std::sync::Arc;

use router::core::config::Config;
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{load_snapshot, AppState};
use router::providers::refresh_task::spawn_background_refresh;
use router::telemetry::logging::init_tracing;
use router::telemetry::request_log::spawn_writer;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = Config::from_env()?;
    let db = init_pool(&cfg.sqlite_path).await?;
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
