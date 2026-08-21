use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::error::AppError;
use crate::core::model::{Pool, PoolMember, PoolStrategy, WireFormat};
use crate::core::state::{reload_snapshot, AppState};
use crate::pools::queries;
use crate::providers::queries as pq;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/pools", get(list).post(create))
        .route(
            "/admin/pools/:id",
            axum::routing::put(update).delete(delete_pool),
        )
        .route(
            "/admin/pools/:id/members",
            get(list_members).put(put_member),
        )
        .route(
            "/admin/pools/:id/members/:provider_id",
            axum::routing::delete(delete_member),
        )
}

async fn list(State(s): State<AppState>) -> Result<Json<Vec<Pool>>, AppError> {
    Ok(Json(queries::list_pools(&s.db).await?))
}

#[derive(Deserialize)]
struct CreatePool {
    id: String,
    wire_format: WireFormat,
    #[serde(default)]
    strategy: PoolStrategy,
    #[serde(default)]
    sticky_limit: Option<i64>,
}

async fn create(
    State(s): State<AppState>,
    Json(b): Json<CreatePool>,
) -> Result<(StatusCode, Json<Pool>), AppError> {
    crate::core::error::validate_path_id(&b.id)?;
    let p = Pool {
        id: b.id,
        wire_format: b.wire_format,
        created_at: Utc::now(),
        strategy: b.strategy,
        sticky_limit: b.sticky_limit,
    };
    queries::insert_pool(&s.db, &p).await?;
    reload_snapshot(&s).await?;
    Ok((StatusCode::CREATED, Json(p)))
}

/// Both fields are optional and independently patchable: an absent field
/// keeps the pool's current value rather than resetting it (in particular,
/// `strategy` must NOT `#[serde(default)]` to `Priority` - a caller PATCHing
/// only `sticky_limit` would otherwise silently clobber an existing
/// `RoundRobin` strategy back to the default). There is no way to explicitly
/// clear `sticky_limit` back to `None` ("use 1") via this endpoint in v1 -
/// send `sticky_limit: 1` for the equivalent behavior.
#[derive(Deserialize, Default)]
struct PoolPatch {
    strategy: Option<PoolStrategy>,
    sticky_limit: Option<i64>,
}

/// Pools have no "edit wire_format" path (fixed at creation, like a
/// provider's OAuth wire format) - this only ever updates strategy/
/// sticky_limit. Clears the pool's rotation cursor on success: a strategy
/// switch (or a sticky_limit change) starting from a stale cursor left
/// over from the pool's prior strategy is confusing, not dangerous (the
/// `% len` guard in `rotate_from_cursor` means it can never panic or go
/// out of range), but starting fresh is the least surprising behavior.
async fn update(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PoolPatch>,
) -> Result<Json<Pool>, AppError> {
    let current = queries::get_pool(&s.db, &id).await?;
    let strategy = b.strategy.unwrap_or(current.strategy);
    let sticky_limit = b.sticky_limit.or(current.sticky_limit);
    queries::update_pool_strategy(&s.db, &id, strategy, sticky_limit).await?;
    reload_snapshot(&s).await?;
    s.pool_rotation.remove(&id);
    Ok(Json(queries::get_pool(&s.db, &id).await?))
}

async fn delete_pool(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    queries::delete_pool(&s.db, &id).await?;
    reload_snapshot(&s).await?;
    s.pool_rotation.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn list_members(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<PoolMember>>, AppError> {
    Ok(Json(queries::list_members(&s.db, &id).await?))
}

#[derive(Deserialize)]
struct PutMember {
    provider_id: String,
    priority: i64,
    #[serde(default)]
    model_override: Option<String>,
}

async fn put_member(
    State(s): State<AppState>,
    Path(pool_id): Path<String>,
    Json(b): Json<PutMember>,
) -> Result<Json<Value>, AppError> {
    queries::get_pool(&s.db, &pool_id).await?;
    pq::get_provider(&s.db, &b.provider_id).await?;
    queries::upsert_member(
        &s.db,
        &PoolMember {
            pool_id: pool_id.clone(),
            provider_id: b.provider_id.clone(),
            priority: b.priority,
            model_override: b.model_override.clone(),
        },
    )
    .await?;
    reload_snapshot(&s).await?;
    Ok(Json(json!({
        "pool_id": pool_id,
        "provider_id": b.provider_id,
        "priority": b.priority,
        "model_override": b.model_override,
    })))
}

async fn delete_member(
    State(s): State<AppState>,
    Path((pool_id, provider_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    queries::delete_member(&s.db, &pool_id, &provider_id).await?;
    reload_snapshot(&s).await?;
    Ok(StatusCode::NO_CONTENT)
}
