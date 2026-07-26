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
    PayloadTooLarge(String),
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
            AppError::PayloadTooLarge(m) => write!(f, "payload too large: {m}"),
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
            AppError::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
            AppError::Db(e) => {
                tracing::warn!(error = %e, "database error while handling request");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, m),
            AppError::Internal(m) => {
                tracing::warn!(error = %m, "internal error while handling request");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": { "message": msg } }))).into_response()
    }
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
            AppError::PayloadTooLarge("x".into())
                .into_response()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            AppError::Internal("x".into()).into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn sqlx_error_converts_into_apperror_db() {
        let e = sqlx::Error::RowNotFound;
        let app: AppError = e.into();
        assert!(matches!(app, AppError::Db(_)));
    }

    #[tokio::test]
    async fn internal_errors_do_not_expose_details_to_clients() {
        let resp = AppError::Internal("secret path /tmp/private.db".into()).into_response();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["message"], "internal server error");
    }
}
