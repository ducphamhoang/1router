use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::SqlitePool;

use crate::admin::auth::rate_limit::LoginAttemptMap;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretOrigin {
    Env,
    SidecarFile,
}

impl SecretOrigin {
    pub fn from_source(source: &crate::core::config::SecretSource) -> Option<Self> {
        match source {
            crate::core::config::SecretSource::Env(_) => Some(SecretOrigin::Env),
            crate::core::config::SecretSource::SidecarFile(_) => Some(SecretOrigin::SidecarFile),
            crate::core::config::SecretSource::BootstrapNeeded => None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    pub shared_secret: Arc<ArcSwap<String>>,
    pub secret_origin: SecretOrigin,
    pub snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    pub runtime: RuntimeStateMap,
    pub log_tx: RequestLogSender,
    pub refresh_locks: RefreshLocks,
    pub login_attempts: LoginAttemptMap,
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
            "SELECT pool_id, provider_id, priority, model_override FROM pool_members
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

/// Make provider-only configurations directly callable after upgrades.
///
/// The public model list is built from pools, so providers imported before the
/// direct-pool flow existed would otherwise be invisible and unroutable.
pub async fn ensure_direct_pools_for_unassigned_providers(
    db: &SqlitePool,
) -> Result<(), AppError> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO pools (id, wire_format, created_at)
         SELECT p.id, p.wire_format, p.created_at
         FROM providers p
         WHERE NOT EXISTS (
             SELECT 1 FROM pool_members m WHERE m.provider_id = p.id
         )
         ON CONFLICT(id) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO pool_members (pool_id, provider_id, priority, model_override)
         SELECT p.id, p.id, 1, NULL
         FROM providers p
         JOIN pools pool ON pool.id = p.id AND pool.wire_format = p.wire_format
         WHERE NOT EXISTS (
             SELECT 1 FROM pool_members m WHERE m.provider_id = p.id
         )
         ON CONFLICT(pool_id, provider_id) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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

    #[tokio::test]
    async fn unassigned_providers_get_direct_pools() {
        let db = init_pool(":memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO providers (id,name,wire_format,kind,upstream_model,created_at,updated_at)
             VALUES ('codex-luna','Codex Luna','openai','oauth_codex','gpt-5','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        )
        .execute(&db)
        .await
        .unwrap();

        ensure_direct_pools_for_unassigned_providers(&db).await.unwrap();

        let snap = load_snapshot(&db).await.unwrap();
        assert_eq!(snap.pools[0].pool.id, "codex-luna");
        assert_eq!(snap.pools[0].members[0].provider_id, "codex-luna");
    }
}
