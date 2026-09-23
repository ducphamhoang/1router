use std::time::Duration;

use crate::admin::auth::session;
use crate::core::state::AppState;

const CLEANUP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Structural mirror of providers::refresh_task::spawn_background_refresh.
pub fn spawn_session_cleanup(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);

        loop {
            interval.tick().await;
            match session::delete_expired(&state.db).await {
                Ok(deleted) if deleted > 0 => {
                    tracing::info!(deleted, "admin session cleanup swept expired rows")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "admin session cleanup sweep failed"),
            }
            // Same cadence, unrelated state: in-memory maps that would
            // otherwise only ever grow (SEC-03, SEC-19).
            let pruned = crate::core::runtime::prune_idle(
                &state.runtime,
                std::time::Instant::now(),
                crate::core::runtime::RUNTIME_IDLE_RETENTION,
            );
            if pruned > 0 {
                tracing::debug!(pruned, "runtime-state sweep");
            }
            crate::admin::auth::rate_limit::prune_stale(&state.login_attempts, std::time::Instant::now());
        }
    });
}
