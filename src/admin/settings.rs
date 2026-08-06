use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::core::config;
use crate::core::error::AppError;
use crate::core::state::{AppState, AuthModeOrigin, SecretOrigin};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/settings/shared-secret",
            get(get_shared_secret).patch(patch_shared_secret),
        )
        .route(
            "/admin/settings/auth-mode",
            get(get_auth_mode).patch(patch_auth_mode),
        )
        .route("/admin/settings/security-status", get(get_security_status))
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

#[derive(Debug, Serialize)]
struct AuthModeResponse {
    require_shared_secret: bool,
    origin: AuthModeOrigin,
}

#[derive(Debug, Deserialize)]
struct AuthModePatch {
    require_shared_secret: bool,
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

/// Whether either fast-path default from onboarding (see
/// `core::config::DEFAULT_SHARED_SECRET`/`DEFAULT_ADMIN_PASSWORD`) is still
/// in place - drives the admin UI's warning banner (frontend App.tsx). Never
/// exposes either secret's value, only these two booleans.
#[derive(Debug, Serialize)]
struct SecurityStatusResponse {
    shared_secret_is_default: bool,
    admin_password_is_default: bool,
    require_shared_secret: bool,
    listen_addr_is_loopback: bool,
}

async fn get_security_status(
    State(s): State<AppState>,
) -> Result<Json<SecurityStatusResponse>, AppError> {
    let shared_secret_is_default = s.shared_secret.load().as_str() == config::DEFAULT_SHARED_SECRET;
    let admin_password_is_default = crate::onboarding::admin_password_is_default(&s.db)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(SecurityStatusResponse {
        shared_secret_is_default,
        admin_password_is_default,
        require_shared_secret: s
            .require_shared_secret
            .load(std::sync::atomic::Ordering::Relaxed),
        listen_addr_is_loopback: config::listen_addr_is_loopback(&s.config.listen_addr),
    }))
}

async fn get_auth_mode(State(s): State<AppState>) -> Result<Json<AuthModeResponse>, AppError> {
    Ok(Json(AuthModeResponse {
        require_shared_secret: s
            .require_shared_secret
            .load(std::sync::atomic::Ordering::Relaxed),
        origin: s.auth_mode_origin,
    }))
}

async fn patch_auth_mode(
    State(s): State<AppState>,
    Json(body): Json<AuthModePatch>,
) -> Result<Json<AuthModeResponse>, AppError> {
    if matches!(s.auth_mode_origin, AuthModeOrigin::Env) {
        return Err(AppError::Conflict(
            "ROUTER_REQUIRE_SHARED_SECRET is set; change or unset the environment variable instead"
                .to_string(),
        ));
    }

    crate::core::settings::set_bool(&s.db, "require_shared_secret", body.require_shared_secret)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    s.require_shared_secret.store(
        body.require_shared_secret,
        std::sync::atomic::Ordering::Relaxed,
    );

    Ok(Json(AuthModeResponse {
        require_shared_secret: body.require_shared_secret,
        origin: s.auth_mode_origin,
    }))
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
