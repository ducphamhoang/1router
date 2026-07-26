use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::SqlitePool;

use crate::core::config::Config;
use crate::core::error::AppError;
use crate::core::model::{LogEntry, Pool, PoolMember, PoolWithMembers, Provider};
use crate::core::runtime::RuntimeStateMap;

#[derive(Clone, Debug)]
pub struct ConfigSnapshot {
    pub providers: Vec<Provider>,
    pub pools: Vec<PoolWithMembers>,
}

pub type RequestLogSender = tokio::sync::mpsc::Sender<LogEntry>;
pub type RefreshLocks = Arc<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    pub snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    pub runtime: RuntimeStateMap,
    pub log_tx: RequestLogSender,
    pub refresh_locks: RefreshLocks,
}

pub async fn load_snapshot(db: &SqlitePool) -> Result<ConfigSnapshot, AppError> {
    let providers: Vec<Provider> =
        sqlx::query_as::<_, Provider>("SELECT * FROM providers ORDER BY name")
            .fetch_all(db)
            .await?;

    let pools: Vec<Pool> = sqlx::query_as::<_, Pool>("SELECT * FROM pools ORDER BY id")
        .fetch_all(db)
        .await?;

    let mut with_members = Vec::with_capacity(pools.len());
    for pool in pools {
        let members: Vec<PoolMember> = sqlx::query_as::<_, PoolMember>(
            "SELECT pool_id, provider_id, priority FROM pool_members
             WHERE pool_id = ? ORDER BY priority ASC",
        )
        .bind(&pool.id)
        .fetch_all(db)
        .await?;
        with_members.push(PoolWithMembers { pool, members });
    }

    Ok(ConfigSnapshot {
        providers,
        pools: with_members,
    })
}

pub async fn reload_snapshot(state: &AppState) -> Result<(), AppError> {
    let snap = load_snapshot(&state.db).await?;
    state.snapshot.store(Arc::new(snap));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;

    #[tokio::test]
    async fn load_snapshot_reads_providers_and_pools() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();

        sqlx::query(
            "INSERT INTO providers (id,name,wire_format,kind,upstream_model,created_at,updated_at)
             VALUES ('p1','P1','openai','passthrough','gpt-4o','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pools (id,wire_format,created_at) VALUES ('gpt-4o','openai','2026-01-01T00:00:00Z')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO pool_members (pool_id,provider_id,priority) VALUES ('gpt-4o','p1',10)",
        )
        .execute(&db)
        .await
        .unwrap();

        let snap = load_snapshot(&db).await.unwrap();
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.pools.len(), 1);
        assert_eq!(snap.pools[0].members.len(), 1);
        assert_eq!(snap.pools[0].members[0].provider_id, "p1");
    }
}
