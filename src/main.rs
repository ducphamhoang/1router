use anyhow::Result;
use std::sync::Arc;

use router::app;
use router::core::config::Config;
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{load_snapshot, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    router::telemetry::logging::init_tracing();

    let cfg = Config::from_env()?;
    let db = init_pool(&cfg.sqlite_path).await?;
    let http = build_client(&cfg);
    let snapshot = load_snapshot(&db).await?;

    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(1024);

    let state = AppState {
        db,
        http,
        config: Arc::new(cfg.clone()),
        snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
    };

    let router = app::build_router(state);
    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    tracing::info!(addr = %cfg.listen_addr, "1router listening");
    axum::serve(listener, router).await?;
    Ok(())
}
