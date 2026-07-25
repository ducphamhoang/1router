use std::sync::Arc;

use crate::core::error::RefreshError;
use crate::core::model::Provider;
use crate::core::state::{AppState, RefreshLocks};
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::providers::queries::upsert_oauth_tokens;

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
}
