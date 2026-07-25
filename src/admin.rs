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
pub async fn import_config(db: &SqlitePool, dump: &ExportDump) -> Result<(), AppError> {
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
        .execute(db)
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
        .execute(db)
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
        .execute(db)
        .await?;
    }
    Ok(())
}
