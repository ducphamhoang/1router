use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::core::config;
use crate::core::error::AppError;
use crate::core::state::{AppState, SecretOrigin};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/admin/settings/shared-secret",
        get(get_shared_secret).patch(patch_shared_secret),
    )
}

#[derive(Debug, Deserialize)]
struct SharedSecretQuery {
    #[serde(default)]
    reveal: bool,
}

#[derive(Debug, Deserialize)]
struct SharedSecretPatch {
    shared_secret: String,
}

#[derive(Debug, Serialize)]
struct SharedSecretResponse {
    shared_secret: String,
    masked: bool,
    origin: SecretOrigin,
}

fn mask_secret(secret: &str) -> String {
    let tail = secret
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();

    if tail.is_empty() {
        "***".to_string()
    } else {
        format!("***{tail}")
    }
}

fn shared_secret_response(
    secret: &str,
    reveal: bool,
    origin: SecretOrigin,
) -> SharedSecretResponse {
    SharedSecretResponse {
        shared_secret: if reveal {
            secret.to_string()
        } else {
            mask_secret(secret)
        },
        masked: !reveal,
        origin,
    }
}

async fn get_shared_secret(
    State(s): State<AppState>,
    Query(q): Query<SharedSecretQuery>,
) -> Result<Json<SharedSecretResponse>, AppError> {
    let secret = s.shared_secret.load();
    Ok(Json(shared_secret_response(
        secret.as_str(),
        q.reveal,
        s.secret_origin,
    )))
}

async fn patch_shared_secret(
    State(s): State<AppState>,
    Json(body): Json<SharedSecretPatch>,
) -> Result<Json<SharedSecretResponse>, AppError> {
    if matches!(s.secret_origin, SecretOrigin::Env) {
        return Err(AppError::Conflict(
            "ROUTER_SHARED_SECRET is set; change or unset the environment variable instead"
                .to_string(),
        ));
    }

    let new_secret = body.shared_secret.trim().to_string();
    if new_secret.is_empty() {
        return Err(AppError::BadRequest(
            "shared_secret must not be empty".to_string(),
        ));
    }

    config::persist_secret(&s.config.sqlite_path, &new_secret)
        .map_err(|e| AppError::Internal(e.to_string()))?;
    s.shared_secret.store(Arc::new(new_secret.clone()));

    Ok(Json(shared_secret_response(
        &new_secret,
        false,
        s.secret_origin,
    )))
}
