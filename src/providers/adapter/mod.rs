pub mod codex;
pub mod passthrough;

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, ProviderKind};

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub provider_data: serde_json::Value,
}

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError>;

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<axum::response::Response, AppError>;

    async fn classify_error(&self, status: StatusCode, headers: &HeaderMap) -> ErrorClass;

    fn needs_refresh(&self, creds: &Credentials) -> bool;

    async fn refresh_credentials(&self, creds: &Credentials) -> Result<Credentials, RefreshError>;
}

pub fn adapter_for(provider: &Provider, http: reqwest::Client) -> Box<dyn ProviderAdapter> {
    match provider.kind {
        ProviderKind::Passthrough => {
            Box::new(passthrough::PassthroughAdapter::new(provider.clone(), http))
        }
        // Replaced by the real Codex adapter in P3-5.
        ProviderKind::OauthCodex => {
            Box::new(passthrough::PassthroughAdapter::new(provider.clone(), http))
        }
    }
}
