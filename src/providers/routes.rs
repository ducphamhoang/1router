use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::error::AppError;
use crate::core::model::{Provider, ProviderKind, WireFormat};
use crate::core::state::{reload_snapshot, AppState};
use crate::providers::queries;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/providers", get(list).post(create))
        .route(
            "/admin/providers/{id}",
            get(get_one).patch(patch).delete(delete),
        )
        .route("/admin/providers/{id}/test", post(test_stub))
        .route("/admin/providers/{id}/state", get(state_stub))
}

fn mask(p: &Provider) -> Value {
    let masked = p.api_key.as_ref().map(|k| {
        let tail = k
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("***{tail}")
    });
    json!({
        "id": &p.id,
        "name": &p.name,
        "wire_format": p.wire_format,
        "kind": p.kind,
        "base_url": &p.base_url,
        "api_key": masked,
        "upstream_model": &p.upstream_model,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

async fn list(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let ps = queries::list_providers(&s.db).await?;
    Ok(Json(Value::Array(ps.iter().map(mask).collect())))
}

async fn get_one(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let p = queries::get_provider(&s.db, &id).await?;
    Ok(Json(mask(&p)))
}

#[derive(Deserialize)]
struct CreateBody {
    id: String,
    name: String,
    wire_format: WireFormat,
    #[serde(default = "default_kind")]
    kind: ProviderKind,
    base_url: Option<String>,
    api_key: Option<String>,
    upstream_model: String,
}

fn default_kind() -> ProviderKind {
    ProviderKind::Passthrough
}

async fn create(
    State(s): State<AppState>,
    Json(b): Json<CreateBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let now = Utc::now();
    let p = Provider {
        id: b.id,
        name: b.name,
        wire_format: b.wire_format,
        kind: b.kind,
        base_url: b.base_url,
        api_key: b.api_key,
        upstream_model: b.upstream_model,
        created_at: now,
        updated_at: now,
    };
    queries::insert_provider(&s.db, &p).await?;
    reload_snapshot(&s).await?;
    Ok((StatusCode::CREATED, Json(mask(&p))))
}

async fn patch(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<queries::ProviderPatch>,
) -> Result<Json<Value>, AppError> {
    let p = queries::update_provider(&s.db, &id, &patch).await?;
    reload_snapshot(&s).await?;
    Ok(Json(mask(&p)))
}

async fn delete(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, AppError> {
    queries::delete_provider(&s.db, &id).await?;
    reload_snapshot(&s).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Filled in later: provider connectivity test + runtime state exposure.
async fn test_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn state_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
