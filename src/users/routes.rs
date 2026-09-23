use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::core::error::AppError;
use crate::core::state::AppState;
use crate::users::queries::{self, User, UserWithKey};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/users", get(list).post(create))
        .route("/admin/users/:id/revoke", post(revoke))
        .route("/admin/users/:id/rotate", post(rotate))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

async fn list(State(s): State<AppState>) -> Result<Json<Vec<User>>, AppError> {
    Ok(Json(queries::list_users(&s.db).await?))
}

async fn create(
    State(s): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<UserWithKey>), AppError> {
    let created = queries::create_user(&s.db, &body.id, body.name.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn revoke(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<User>, AppError> {
    Ok(Json(queries::revoke_user(&s.db, &id).await?))
}

async fn rotate(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<UserWithKey>, AppError> {
    Ok(Json(queries::rotate_user_key(&s.db, &id).await?))
}
