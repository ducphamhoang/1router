use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::core::error::AppError;
use crate::core::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/stats", get(overall))
        .route("/admin/stats/pools/:id", get(per_pool))
        .route("/admin/stats/callers", get(per_caller))
}

async fn overall(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let row: (i64, i64) =
        sqlx::query_as("SELECT count(*), coalesce(sum(success),0) FROM request_log")
            .fetch_one(&s.db)
            .await?;
    let total = row.0;
    let successes = row.1;
    Ok(Json(json!({
        "total": total,
        "successes": successes,
        "failures": total - successes,
    })))
}

/// Request counts grouped by who made them: one row per client key
/// (`caller_key_id` + its name), plus `"admin"` for the shared secret and a
/// `null` name for anonymous open-access calls.
async fn per_caller(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    // (caller_key_id, caller_name, total, successes, last created_at)
    type Row = (Option<String>, Option<String>, i64, i64, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT caller_key_id, caller_name, count(*), coalesce(sum(success),0), max(created_at)
         FROM request_log GROUP BY caller_key_id, caller_name ORDER BY count(*) DESC",
    )
    .fetch_all(&s.db)
    .await?;

    let callers: Vec<Value> = rows
        .into_iter()
        .map(|(key_id, name, total, ok, last)| {
            json!({
                "caller_key_id": key_id,
                "caller_name": name,
                "total": total,
                "successes": ok,
                "failures": total - ok,
                "last_request_at": last,
            })
        })
        .collect();

    Ok(Json(json!({ "callers": callers })))
}

async fn per_pool(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT provider_id, count(*), coalesce(sum(success),0)
         FROM request_log WHERE pool_id = ? GROUP BY provider_id",
    )
    .bind(&id)
    .fetch_all(&s.db)
    .await?;

    let providers: Vec<Value> = rows
        .into_iter()
        .map(|(pid, total, ok)| {
            json!({
                "provider_id": pid,
                "total": total,
                "successes": ok,
                "failures": total - ok,
                "success_rate": if total > 0 { ok as f64 / total as f64 } else { 0.0 },
            })
        })
        .collect();

    Ok(Json(json!({ "pool_id": id, "providers": providers })))
}
