pub mod codex;
pub mod commandcode;
pub mod http;

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{OAuthState, Provider, ProviderKind, WireFormat};

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub provider_data: serde_json::Value,
}

impl Credentials {
    pub fn from_provider_and_oauth(provider: &Provider, oauth: Option<OAuthState>) -> Self {
        match oauth {
            Some(oauth) => Self {
                api_key: provider.api_key.clone(),
                access_token: oauth.access_token,
                refresh_token: oauth.refresh_token,
                id_token: oauth.id_token,
                access_expires_at: oauth.access_expires_at,
                provider_data: oauth.provider_data,
            },
            None => Self {
                api_key: provider.api_key.clone(),
                ..Default::default()
            },
        }
    }
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
    adapter_for_wire(provider, http, provider.wire_format)
}

pub fn adapter_for_wire(
    provider: &Provider,
    http: reqwest::Client,
    client_wire: WireFormat,
) -> Box<dyn ProviderAdapter> {
    match provider.kind {
        ProviderKind::Passthrough => Box::new(http::HttpAdapter::new(
            provider.clone(),
            http,
            client_wire,
        )),
        ProviderKind::OauthCodex => Box::new(codex::CodexAdapter::new(
            provider.clone(),
            http,
            client_wire,
        )),
        ProviderKind::OauthCommandCode => Box::new(commandcode::CommandCodeAdapter::new(
            provider.clone(),
            http,
            client_wire,
        )),
    }
}
