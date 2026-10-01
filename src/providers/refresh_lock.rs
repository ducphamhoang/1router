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

/// [`refresh_and_persist`] under the provider's refresh lock, on its own task.
///
/// A refresh spends the single-use refresh token upstream before the new one
/// is written to the DB. Run inline in a request future, a client disconnect
/// (or timeout) in between drops the persist step: the rotated token is lost,
/// the DB keeps the spent one, and the next refresh gets invalid_grant, taking
/// the provider down until an admin re-authenticates. The spawned task is not
/// cancelled with the caller, so the rotation always reaches the DB.
pub async fn refresh_and_persist_detached(
    state: &AppState,
    provider: &Provider,
    adapter: Arc<dyn ProviderAdapter>,
    creds: &Credentials,
) -> Result<Credentials, RefreshError> {
    let (state, provider, creds) = (state.clone(), provider.clone(), creds.clone());
    tokio::spawn(async move {
        with_refresh_lock(&state.refresh_locks, &provider.id, || async {
            refresh_and_persist(&state, &provider, adapter.as_ref(), &creds).await
        })
        .await
    })
    .await
    .unwrap_or_else(|e| Err(RefreshError::Transient(format!("refresh task failed: {e}"))))
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
    // The old refresh token is already spent upstream, so a failed write here
    // loses the only valid one: retry a transient DB error before giving up.
    let mut attempt = 0;
    loop {
        let persisted = upsert_oauth_tokens(
            &state.db,
            &provider.id,
            new_creds.access_token.as_deref(),
            new_creds.refresh_token.as_deref(),
            new_creds.id_token.as_deref(),
            new_creds.access_expires_at,
            &new_creds.provider_data,
        )
        .await;
        match persisted {
            Ok(_) => return Ok(new_creds),
            Err(e) if attempt < PERSIST_RETRIES => {
                attempt += 1;
                tracing::warn!(provider = %provider.id, error = %e, attempt, "persist refreshed tokens failed, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
            }
            Err(e) => {
                tracing::error!(provider = %provider.id, error = %e, "persist refreshed tokens failed; provider will need re-auth");
                return Err(RefreshError::Transient(format!("persist refreshed tokens: {e}")));
            }
        }
    }
}

const PERSIST_RETRIES: u32 = 3;

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
            dataset_log_dir: std::path::PathBuf::from("dataset-logs"),
            media: Default::default(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (dataset_tx, _dataset_rx) = tokio::sync::mpsc::channel(8);
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
            dataset_log_tx: dataset_tx,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            login_attempts: Arc::new(dashmap::DashMap::new()),
            discovered_models: Arc::new(dashmap::DashMap::new()),
            pool_rotation: Arc::new(dashmap::DashMap::new()),
            media: Default::default(),
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
            dataset_logging: false,
            default_reasoning_effort: None,
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

    // v12 regression: the caller's future is dropped while the upstream
    // refresh is in flight (client disconnect). The rotated token must still
    // be persisted rather than lost with the request.
    struct GatedAdapter {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for GatedAdapter {
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
            self.started.notify_one();
            self.release.notified().await;
            Ok(Credentials {
                access_token: Some("at-rotated".into()),
                refresh_token: Some("rt-rotated".into()),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn rotated_token_is_persisted_even_if_caller_is_dropped() {
        let state = test_app_state().await;
        let provider = test_provider();
        crate::providers::queries::insert_provider(&state.db, &provider)
            .await
            .unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(GatedAdapter {
            started: started.clone(),
            release: release.clone(),
        });
        let creds = Credentials {
            access_token: Some("at-old".into()),
            refresh_token: Some("rt-old".into()),
            ..Default::default()
        };

        // Drop the caller once the upstream refresh has started.
        tokio::select! {
            _ = refresh_and_persist_detached(&state, &provider, adapter, &creds) => {
                panic!("refresh should still be blocked upstream")
            }
            _ = started.notified() => {}
        }
        release.notify_one();

        let persisted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(Some(os)) = get_oauth_state(&state.db, &provider.id).await {
                    if os.refresh_token.as_deref() == Some("rt-rotated") {
                        return os;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("rotated refresh token was lost with the dropped caller");
        assert_eq!(persisted.access_token.as_deref(), Some("at-rotated"));
    }
}
