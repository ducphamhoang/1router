pub mod auth;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::model::{Pool, PoolMember, Provider};
use crate::core::state::{reload_snapshot, AppState};
use crate::pools::queries as pools_q;
use crate::providers::queries as prov_q;

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportDump {
    pub providers: Vec<Provider>,
    pub pools: Vec<Pool>,
    pub members: Vec<PoolMember>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/export", get(export))
        .route("/admin/import", post(import))
}

async fn export(State(s): State<AppState>) -> Result<Json<ExportDump>, AppError> {
    let providers = prov_q::list_providers(&s.db).await?;
    let pools = pools_q::list_pools(&s.db).await?;
    let mut members = Vec::new();
    for p in &pools {
        members.extend(pools_q::list_members(&s.db, &p.id).await?);
    }
    Ok(Json(ExportDump {
        providers,
        pools,
        members,
    }))
}

async fn import(
    State(s): State<AppState>,
    Json(dump): Json<ExportDump>,
) -> Result<Json<serde_json::Value>, AppError> {
    import_config(&s.db, &dump).await?;
    reload_snapshot(&s).await?;
    Ok(Json(serde_json::json!({
        "imported": {
            "providers": dump.providers.len(),
            "pools": dump.pools.len(),
            "members": dump.members.len(),
        }
    })))
}

/// Idempotent upsert import; reused verbatim by first-boot seeding (P4-2).
///
/// This is a backup/restore artifact: exported and imported providers include
/// the real `api_key`, unlike masked API responses.
///
/// All-or-nothing: runs inside a single transaction so a crash (or a bad row)
/// partway through can't leave a half-seeded config (e.g. providers inserted
/// but no pools/members) that first-boot seeding's "providers table
/// non-empty" guard would then treat as already-seeded and never retry.
pub async fn import_config(db: &SqlitePool, dump: &ExportDump) -> Result<(), AppError> {
    let mut tx = db.begin().await?;

    for p in &dump.providers {
        sqlx::query(
            "INSERT INTO providers (id,name,wire_format,kind,base_url,api_key,upstream_model,created_at,updated_at)
             VALUES (?,?,?,?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name, wire_format=excluded.wire_format, kind=excluded.kind,
               base_url=excluded.base_url, api_key=excluded.api_key,
               upstream_model=excluded.upstream_model, updated_at=excluded.updated_at",
        )
        .bind(&p.id)
        .bind(&p.name)
        .bind(p.wire_format)
        .bind(p.kind)
        .bind(&p.base_url)
        .bind(&p.api_key)
        .bind(&p.upstream_model)
        .bind(p.created_at)
        .bind(p.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for pool in &dump.pools {
        sqlx::query(
            "INSERT INTO pools (id, wire_format, created_at) VALUES (?,?,?)
             ON CONFLICT(id) DO UPDATE SET wire_format=excluded.wire_format",
        )
        .bind(&pool.id)
        .bind(pool.wire_format)
        .bind(pool.created_at)
        .execute(&mut *tx)
        .await?;
    }
    for m in &dump.members {
        sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority) VALUES (?,?,?)
             ON CONFLICT(pool_id, provider_id) DO UPDATE SET priority=excluded.priority",
        )
        .bind(&m.pool_id)
        .bind(&m.provider_id)
        .bind(m.priority)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use crate::core::model::{Pool, PoolMember, Provider, ProviderKind, WireFormat};
    use chrono::Utc;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough,
            base_url: Some("https://x".into()),
            api_key: Some("k".into()),
            upstream_model: "m".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    // Regression test for the Phase 4 review: import_config previously ran
    // each insert autocommitted, so a bad row partway through (e.g. a member
    // referencing a nonexistent provider) could leave providers/pools
    // committed with no members - a half-seeded state that first-boot
    // seeding's "providers table non-empty" guard would then treat as
    // already-seeded and never retry. Must be all-or-nothing.
    #[tokio::test]
    async fn import_is_all_or_nothing_on_failure() {
        let db = init_pool(":memory:").await.unwrap();
        let dump = ExportDump {
            providers: vec![provider("p1")],
            pools: vec![Pool {
                id: "gpt-4o".into(),
                wire_format: WireFormat::OpenAi,
                created_at: Utc::now(),
            }],
            // references a provider that was never inserted - FK violation
            members: vec![PoolMember {
                pool_id: "gpt-4o".into(),
                provider_id: "does-not-exist".into(),
                priority: 1,
            }],
        };

        let result = import_config(&db, &dump).await;
        assert!(result.is_err(), "expected the FK violation to error out");

        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n.0, 0, "providers insert should have been rolled back too");
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM pools")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n.0, 0, "pools insert should have been rolled back too");
    }
}
