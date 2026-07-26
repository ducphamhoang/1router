use std::time::Duration;

use crate::core::error::RefreshError;
use crate::core::model::ProviderKind;
use crate::core::state::{load_snapshot, AppState};
use crate::providers::adapter::{adapter_for, Credentials};
use crate::providers::queries::get_oauth_state;
use crate::providers::refresh_lock::{refresh_and_persist, with_refresh_lock};

const TICK: Duration = Duration::from_secs(6 * 60 * 60); // every 6 hours

pub fn spawn_background_refresh(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        loop {
            interval.tick().await;
            refresh_due_providers(&state).await;
        }
    });
}

pub async fn refresh_due_providers(state: &AppState) {
    let snapshot = match load_snapshot(&state.db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "background refresh: snapshot load failed");
            return;
        }
    };

    for provider in snapshot.providers.iter() {
        if !matches!(provider.kind, ProviderKind::OauthCodex) {
            continue;
        }
        let os = match get_oauth_state(&state.db, &provider.id).await {
            Ok(Some(os)) => os,
            _ => continue,
        };
        let creds = Credentials {
            api_key: None,
            access_token: os.access_token,
            refresh_token: os.refresh_token,
            id_token: os.id_token,
            access_expires_at: os.access_expires_at,
            provider_data: os.provider_data,
        };
        let adapter = adapter_for(provider, state.http.clone());
        if !adapter.needs_refresh(&creds) {
            continue;
        }
        let result = with_refresh_lock(&state.refresh_locks, &provider.id, || async {
            refresh_and_persist(state, provider, adapter.as_ref(), &creds).await
        })
        .await;
        match result {
            Ok(_) => tracing::info!(provider = %provider.id, "background token refresh ok"),
            Err(RefreshError::InvalidGrant) => {
                // Permanent failure - the refresh token is dead and re-auth is required.
                // Mirrors the reactive refresh path in proxy::flow so a background-
                // discovered invalid_grant is just as visible via /admin/providers/:id/state
                // as one discovered by a live request.
                state
                    .runtime
                    .entry(provider.id.clone())
                    .or_default()
                    .mark_misconfigured();
                tracing::warn!(provider = %provider.id, "background token refresh: invalid_grant, re-auth required");
            }
            Err(e) => {
                tracing::warn!(provider = %provider.id, error = %e, "background token refresh failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use std::sync::Arc;
    use std::time::Duration;

    async fn state_with(db: sqlx::SqlitePool) -> AppState {
        let cfg = crate::core::config::Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "s".into(),
            seed_path: None,
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        AppState {
            db,
            http: reqwest::Client::new(),
            shared_secret: Arc::new(arc_swap::ArcSwap::from_pointee(cfg.shared_secret.clone())),
            config: Arc::new(cfg),
            secret_origin: SecretOrigin::SidecarFile,
            snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(ConfigSnapshot {
                providers: vec![],
                pools: vec![],
            })),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx: tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
        }
    }

    #[tokio::test]
    async fn refresh_due_providers_no_codex_is_noop() {
        let db = init_pool(":memory:").await.unwrap();
        let state = state_with(db).await;
        // No oauth_codex providers -> should complete without error/panic.
        refresh_due_providers(&state).await;
    }
}
