use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Utc;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::Provider;
use crate::providers::adapter::codex::refresh;
use crate::providers::adapter::codex::transform;
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct CodexAdapter {
    provider: Provider,
    http: reqwest::Client,
}

impl CodexAdapter {
    pub fn new(provider: Provider, http: reqwest::Client) -> Self {
        Self { provider, http }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for CodexAdapter {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError> {
        let client_json: serde_json::Value = serde_json::from_slice(client_body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;
        // session id: prompt_cache_key ties to a stable per-provider id for now.
        let session_id = format!("1router-{}", self.provider.id);
        let mut transformed = transform::transform_request(&client_json, &session_id);
        // The client's `model` is the pool id, not a real Codex model name -
        // rewrite to the provider's actual upstream model, matching
        // PassthroughAdapter's behavior (confirmed via a real-account 400:
        // "'<pool-id>' model is not supported when using Codex with a ChatGPT
        // account").
        if let Some(obj) = transformed.as_object_mut() {
            obj.insert(
                "model".into(),
                serde_json::Value::String(self.provider.upstream_model.clone()),
            );
        }

        let account_id = creds.provider_data["chatgpt_account_id"]
            .as_str()
            .unwrap_or_default();
        let access = creds
            .access_token
            .as_ref()
            .ok_or_else(|| AppError::Internal("codex provider missing access_token".into()))?;

        let mut builder = self
            .http
            .post(RESPONSES_URL)
            .json(&transformed)
            .bearer_auth(access)
            .header("originator", "codex_cli_rs")
            .header("User-Agent", format!("codex_cli_rs/{CODEX_VERSION}"));
        if !account_id.is_empty() {
            builder = builder.header("ChatGPT-Account-ID", account_id);
        }
        builder
            .build()
            .map_err(|e| AppError::Internal(format!("codex request build failed: {e}")))
    }

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        if client_wanted_stream {
            // Responses-API SSE events (response.created, output_text.delta,
            // function_call_arguments.delta, ...) don't match what an
            // OpenAI-Chat-Completions-compatible client expects on the wire
            // (chat.completion.chunk / delta.content / delta.tool_calls) -
            // translate event-by-event instead of passing the body through.
            use futures::StreamExt;
            let stream = upstream.bytes_stream().boxed();
            let converted =
                transform::convert_sse_stream(stream, self.provider.upstream_model.clone());
            return Ok((status, Body::from_stream(converted)).into_response());
        }
        // aggregate: client did not ask to stream, but Codex is forced to stream upstream
        let text = upstream
            .text()
            .await
            .map_err(|e| AppError::Upstream(format!("codex sse read: {e}")))?;
        if let Some(err_type) = transform::sse_embedded_error(&text) {
            return Err(AppError::Upstream(format!(
                "codex embedded error: {err_type}"
            )));
        }
        let json = transform::aggregate_sse(&text);
        Ok((StatusCode::OK, axum::Json(json)).into_response())
    }

    async fn classify_error(&self, status: StatusCode, headers: &HeaderMap) -> ErrorClass {
        backoff::classify(status, headers)
    }

    fn needs_refresh(&self, creds: &Credentials) -> bool {
        refresh::needs_refresh(creds, Utc::now())
    }

    async fn refresh_credentials(&self, creds: &Credentials) -> Result<Credentials, RefreshError> {
        let rt = creds
            .refresh_token
            .as_ref()
            .ok_or(RefreshError::InvalidGrant)?;
        let tokens = refresh::refresh_tokens(&self.http, rt).await?;
        Ok(Credentials {
            api_key: None,
            access_token: Some(tokens.access_token),
            refresh_token: tokens.refresh_token.or_else(|| creds.refresh_token.clone()),
            id_token: tokens.id_token.or_else(|| creds.id_token.clone()),
            access_expires_at: tokens
                .expires_in
                .map(|s| Utc::now() + chrono::Duration::seconds(s)),
            provider_data: creds.provider_data.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use crate::providers::adapter::{Credentials, ProviderAdapter};
    use bytes::Bytes;
    use chrono::Utc;

    fn prov() -> Provider {
        Provider {
            id: "cx".into(),
            name: "Codex".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::OauthCodex,
            base_url: None,
            api_key: None,
            upstream_model: "gpt-5-codex".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn creds() -> Credentials {
        Credentials {
            access_token: Some("at-123".into()),
            provider_data: serde_json::json!({ "chatgpt_account_id": "acct_9" }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn build_request_targets_responses_api_with_headers() {
        let a = CodexAdapter::new(prov(), reqwest::Client::new());
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o", "messages": [], "temperature": 0.5
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert_eq!(
            req.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer at-123");
        assert_eq!(req.headers().get("chatgpt-account-id").unwrap(), "acct_9");
        assert_eq!(req.headers().get("originator").unwrap(), "codex_cli_rs");
        // allowlist removed temperature
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(sent.get("temperature").is_none());
        assert_eq!(sent["stream"], true);
        // the client's `model` (a pool id, e.g. "gpt-4o") is not a real Codex
        // model - it must be rewritten to the provider's upstream_model.
        assert_eq!(sent["model"], "gpt-5-codex");
    }
}
