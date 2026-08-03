use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::error::AppError;
use crate::core::model::{Pool, PoolMember, WireFormat};
use crate::core::state::{reload_snapshot, AppState};
use crate::pools::queries;
use crate::providers::queries as pq;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/pools", get(list).post(create))
        .route("/admin/pools/:id", axum::routing::delete(delete_pool))
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
}

async fn create(
    State(s): State<AppState>,
    Json(b): Json<CreatePool>,
) -> Result<(StatusCode, Json<Pool>), AppError> {
    let p = Pool {
        id: b.id,
        wire_format: b.wire_format,
        created_at: Utc::now(),
    };
    queries::insert_pool(&s.db, &p).await?;
    reload_snapshot(&s).await?;
    Ok((StatusCode::CREATED, Json(p)))
}

async fn delete_pool(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    queries::delete_pool(&s.db, &id).await?;
    reload_snapshot(&s).await?;
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
    let pool = queries::get_pool(&s.db, &pool_id).await?;
    let provider = pq::get_provider(&s.db, &b.provider_id).await?;
    if !matches!(
        (pool.wire_format, provider.wire_format),
        (WireFormat::OpenAi, WireFormat::OpenAi) | (WireFormat::Anthropic, WireFormat::Anthropic)
    ) {
        return Err(AppError::BadRequest(
            "provider wire_format does not match pool wire_format".into(),
        ));
    }
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
