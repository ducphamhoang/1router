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
        }
    });
}
