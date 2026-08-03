use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    NotFound,
    BadRequest(String),
    Unauthorized,
    Conflict(String),
    Db(sqlx::Error),
    Upstream(String),
    Internal(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::NotFound => write!(f, "not found"),
            AppError::BadRequest(m) => write!(f, "bad request: {m}"),
            AppError::Unauthorized => write!(f, "unauthorized"),
            AppError::Conflict(m) => write!(f, "conflict: {m}"),
            AppError::Db(e) => write!(f, "db error: {e}"),
            AppError::Upstream(m) => write!(f, "upstream error: {m}"),
            AppError::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Db(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            AppError::Conflict(m) => (StatusCode::CONFLICT, m),
            AppError::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, m),
            AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({ "error": { "message": msg } }))).into_response()
    }
}

/// `id` becomes a URL path segment everywhere (`/admin/pools/:id`,
/// `/admin/providers/:id/...`), so an empty or `/`-containing id can be
/// created via the JSON body (nothing stops that at the type level) but can
/// then never be addressed again - matchit won't route an empty segment,
/// and a literal `/` inside one just becomes extra path segments. Reject
/// both at creation instead of leaving an unreachable row behind.
pub fn validate_path_id(id: &str) -> Result<(), AppError> {
    if id.trim().is_empty() {
        return Err(AppError::BadRequest("id must not be empty".into()));
    }
    if id.contains('/') {
        return Err(AppError::BadRequest("id must not contain '/'".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    Success,
    Retryable { retry_after: Option<Duration> },
    NonRetryable,
    AuthExpired,
}

#[derive(Debug)]
pub enum RefreshError {
    InvalidGrant,
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::InvalidGrant => write!(f, "invalid_grant"),
            RefreshError::Transient(m) => write!(f, "transient refresh error: {m}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    #[test]
    fn app_error_maps_to_status_codes() {
        assert_eq!(
            AppError::NotFound.into_response().status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            AppError::Unauthorized.into_response().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            AppError::BadRequest("x".into()).into_response().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::Conflict("x".into()).into_response().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            AppError::Internal("x".into()).into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn validate_path_id_rejects_empty_or_slash_containing_ids() {
        assert!(validate_path_id("codex-sol").is_ok());
        assert!(validate_path_id("").is_err());
        assert!(validate_path_id("   ").is_err());
        assert!(validate_path_id("a/b").is_err());
    }

    #[test]
    fn sqlx_error_converts_into_apperror_db() {
        let e = sqlx::Error::RowNotFound;
        let app: AppError = e.into();
        assert!(matches!(app, AppError::Db(_)));
    }
}
