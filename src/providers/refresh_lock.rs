use std::sync::Arc;

use crate::core::error::RefreshError;
use crate::core::model::Provider;
use crate::core::state::{AppState, RefreshLocks};
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::providers::queries::{get_oauth_state, upsert_oauth_tokens};

pub async fn with_refresh_lock<F, Fut, T>(locks: &RefreshLocks, provider_id: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let lock = locks
        .entry(provider_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    f().await
}

pub async fn refresh_and_persist(
    state: &AppState,
    provider: &Provider,
    adapter: &dyn ProviderAdapter,
    creds: &Credentials,
) -> Result<Credentials, RefreshError> {
    // Re-read the persisted state now that we hold the lock: another waiter
    // (the reactive path or a background tick) may have already refreshed while
    // we were waiting to acquire it. Refresh tokens are single-use, so retrying
    // with our now-stale `creds` would spend an already-spent token and fail
    // with invalid_grant - reuse the fresh result instead of refreshing again.
    if let Ok(Some(current)) = get_oauth_state(&state.db, &provider.id).await {
        if current.access_token.is_some() && current.access_token != creds.access_token {
            return Ok(Credentials::from_provider_and_oauth(
                provider,
                Some(current),
            ));
        }
    }

    let new_creds = adapter.refresh_credentials(creds).await?;
    upsert_oauth_tokens(
        &state.db,
        &provider.id,
        new_creds.access_token.as_deref(),
        new_creds.refresh_token.as_deref(),
        new_creds.id_token.as_deref(),
        new_creds.access_expires_at,
        &new_creds.provider_data,
    )
    .await
    .map_err(|e| RefreshError::Transient(format!("persist refreshed tokens: {e}")))?;
    Ok(new_creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn lock_serializes_same_provider() {
        let locks: crate::core::state::RefreshLocks = Arc::new(dashmap::DashMap::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..5 {
            let l = locks.clone();
            let c = counter.clone();
            let m = max_seen.clone();
            handles.push(tokio::spawn(async move {
                with_refresh_lock(&l, "p1", || async move {
                    let cur = c.fetch_add(1, Ordering::SeqCst) + 1;
                    m.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    c.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // never more than one concurrent critical section for the same provider
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    // Regression test for the Phase 3 review's Critical finding: a waiter that
    // acquires the lock after another refresh already completed must reuse the
    // fresh persisted credentials instead of calling refresh_credentials again
    // with its now-stale (already-spent) refresh token.
    struct CountingAdapter {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for CountingAdapter {
        async fn build_request(
            &self,
            _client_body: &bytes::Bytes,
            _creds: &Credentials,
        ) -> Result<reqwest::Request, crate::core::error::AppError> {
            unimplemented!()
        }
        async fn transform_response(
            &self,
            _upstream: reqwest::Response,
            _client_wanted_stream: bool,
        ) -> Result<axum::response::Response, crate::core::error::AppError> {
            unimplemented!()
        }
        async fn classify_error(
            &self,
            _status: axum::http::StatusCode,
            _headers: &axum::http::HeaderMap,
        ) -> crate::core::error::ErrorClass {
            unimplemented!()
        }
        fn needs_refresh(&self, _creds: &Credentials) -> bool {
            true
        }
        async fn refresh_credentials(
            &self,
            _creds: &Credentials,
        ) -> Result<Credentials, RefreshError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Credentials {
                access_token: Some(format!("at-{n}")),
                refresh_token: Some(format!("rt-{n}")),
                ..Default::default()
            })
        }
    }

    async fn test_app_state() -> AppState {
        let db = crate::core::db::init_pool(":memory:").await.unwrap();
        let cfg = crate::core::config::Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "s".into(),
            seed_path: None,
            connect_timeout: std::time::Duration::from_secs(1),
            ttfb_timeout: std::time::Duration::from_secs(1),
            idle_timeout: std::time::Duration::from_secs(1),
            max_body_bytes: 1024,
            drain_timeout: std::time::Duration::from_secs(1),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        AppState {
            http: reqwest::Client::new(),
            shared_secret: Arc::new(arc_swap::ArcSwap::from_pointee(cfg.shared_secret.clone())),
            config: Arc::new(cfg),
            secret_origin: crate::core::state::SecretOrigin::SidecarFile,
            require_shared_secret: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            auth_mode_origin: crate::core::state::AuthModeOrigin::Default,
            snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::core::state::ConfigSnapshot {
                    providers: vec![],
                    pools: vec![],
                },
            )),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx: tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            login_attempts: Arc::new(dashmap::DashMap::new()),
            discovered_models: Arc::new(dashmap::DashMap::new()),
            db,
        }
    }

    fn test_provider() -> Provider {
        Provider {
            id: "cx".into(),
            name: "Codex".into(),
            wire_format: crate::core::model::WireFormat::OpenAi,
            kind: crate::core::model::ProviderKind::OauthCodex,
            base_url: None,
            api_key: None,
            upstream_model: "m".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn waiter_reuses_fresh_credentials_instead_of_double_refreshing() {
        let state = test_app_state().await;
        let provider = test_provider();
        crate::providers::queries::insert_provider(&state.db, &provider)
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let adapter = CountingAdapter {
            calls: calls.clone(),
        };
        let stale_creds = Credentials {
            access_token: Some("at-stale".into()),
            refresh_token: Some("rt-stale".into()),
            ..Default::default()
        };

        // First call: nothing persisted yet, so it really refreshes and persists.
        let first = refresh_and_persist(&state, &provider, &adapter, &stale_creds)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call still carries the ORIGINAL stale creds (as a delayed waiter
        // would, since it captured creds before the first refresh completed) -
        // it must detect the persisted state has moved on and reuse it, NOT call
        // refresh_credentials again with the stale (already-spent) refresh token.
        let second = refresh_and_persist(&state, &provider, &adapter, &stale_creds)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second waiter must not re-refresh"
        );
        assert_eq!(second.access_token, first.access_token);
    }
}
