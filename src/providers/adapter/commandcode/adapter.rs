use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::providers::adapter::codex::claude_bridge;
use crate::providers::adapter::commandcode::transform;
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const GENERATE_URL: &str = "https://api.commandcode.ai/alpha/generate";
pub const DEFAULT_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";
const COMMAND_CODE_VERSION: &str = "0.29.0";

pub struct CommandCodeAdapter {
    provider: Provider,
    http: reqwest::Client,
    client_wire: WireFormat,
}

impl CommandCodeAdapter {
    pub fn new(provider: Provider, http: reqwest::Client, client_wire: WireFormat) -> Self {
        Self {
            provider,
            http,
            client_wire,
        }
    }
}

fn generate_url() -> String {
    std::env::var("ROUTER_COMMANDCODE_BASE_URL")
        .map(|base| format!("{}/alpha/generate", base.trim_end_matches('/')))
        .unwrap_or_else(|_| GENERATE_URL.to_string())
}

#[async_trait::async_trait]
impl ProviderAdapter for CommandCodeAdapter {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError> {
        let json: Value = serde_json::from_slice(client_body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;
        let json = match self.client_wire {
            WireFormat::Anthropic => claude_bridge::claude_to_openai_request(&json),
            WireFormat::OpenAi => json,
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cwd_string = cwd.to_string_lossy().to_string();
        let mut envelope =
            transform::transform_request(&json, &Uuid::new_v4().to_string(), &cwd_string);
        envelope["params"]["model"] = Value::String(self.provider.upstream_model.clone());
        let access = creds.access_token.as_ref().ok_or_else(|| {
            AppError::Internal("commandcode provider missing access_token".into())
        })?;
        self.http
            .post(generate_url())
            .json(&envelope)
            .bearer_auth(access)
            .header("x-command-code-version", COMMAND_CODE_VERSION)
            .header("x-cli-environment", "production")
            .header(
                "x-project-slug",
                transform::project_slug_from_path(&cwd_string),
            )
            .header("x-taste-learning", "true")
            .header("x-co-flag", "false")
            .build()
            .map_err(|e| AppError::Internal(format!("commandcode request build failed: {e}")))
    }

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        if client_wanted_stream {
            use futures::StreamExt;
            let openai = transform::convert_ndjson_stream(
                upstream.bytes_stream().boxed(),
                self.provider.upstream_model.clone(),
            );
            if self.client_wire == WireFormat::Anthropic {
                let claude = claude_bridge::convert_openai_sse_to_claude_sse(openai);
                let mut response = (status, Body::from_stream(claude)).into_response();
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                return Ok(response);
            }
            let mut response = (status, Body::from_stream(openai)).into_response();
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            return Ok(response);
        }
        let body = upstream
            .text()
            .await
            .map_err(|e| AppError::Upstream(format!("commandcode response read: {e}")))?;
        if let Some(error) = transform::ndjson_embedded_error(&body) {
            return Err(AppError::Upstream(format!(
                "commandcode embedded error: {error}"
            )));
        }
        let json = transform::aggregate_ndjson(&body, &self.provider.upstream_model);
        if self.client_wire == WireFormat::Anthropic {
            return Ok((
                StatusCode::OK,
                axum::Json(claude_bridge::openai_json_to_claude_message(&json)),
            )
                .into_response());
        }
        Ok((StatusCode::OK, axum::Json(json)).into_response())
    }

    async fn classify_error(&self, status: StatusCode, headers: &HeaderMap) -> ErrorClass {
        backoff::classify(status, headers)
    }

    fn needs_refresh(&self, _creds: &Credentials) -> bool {
        false
    }

    async fn refresh_credentials(&self, creds: &Credentials) -> Result<Credentials, RefreshError> {
        // Command Code keys do not expire; refresh_task and AuthExpired
        // recovery intentionally exclude this provider kind.
        Ok(creds.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::AppError;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use crate::providers::adapter::{Credentials, ProviderAdapter};
    use bytes::Bytes;
    use chrono::Utc;

    fn prov() -> Provider {
        Provider {
            id: "cc".into(),
            name: "Command Code".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::OauthCommandCode,
            base_url: None,
            api_key: None,
            upstream_model: "cc-model".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
    fn creds() -> Credentials {
        Credentials {
            access_token: Some("cc-key-123".into()),
            ..Default::default()
        }
    }
    fn response(body: &str) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(Bytes::from(body.to_owned()))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn build_request_targets_generate_with_fixed_headers() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[],"stream":true}))
                .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/alpha/generate"
        );
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.headers()["authorization"], "Bearer cc-key-123");
        assert_eq!(req.headers()["x-command-code-version"], "0.29.0");
        assert_eq!(req.headers()["x-cli-environment"], "production");
        assert!(req.headers().get("x-project-slug").is_some());
        assert_eq!(req.headers()["x-taste-learning"], "true");
        assert_eq!(req.headers()["x-co-flag"], "false");
    }

    #[tokio::test]
    async fn build_request_rewrites_model_to_the_upstream_model() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[]})).unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["params"]["model"], "cc-model");
    }

    #[tokio::test]
    async fn build_request_uses_access_token_not_api_key() {
        let mut p = prov();
        p.api_key = Some("WRONG".into());
        let a = CommandCodeAdapter::new(p, reqwest::Client::new(), WireFormat::OpenAi);
        let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"messages":[]})).unwrap());
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(req.headers()["authorization"], "Bearer cc-key-123");
        assert!(matches!(
            a.build_request(&body, &Credentials::default()).await,
            Err(AppError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn build_request_bridges_an_anthropic_client_body() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::Anthropic);
        let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"model":"pool","system":"be terse","messages":[{"role":"user","content":"hi"}],"max_tokens":64})).unwrap());
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["params"]["system"], "be terse");
        assert_eq!(sent["params"]["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn transform_response_streaming_openai_wire_emits_framed_sse() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let response = a.transform_response(response("{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n"), true).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("data: "));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn transform_response_streaming_anthropic_wire_emits_claude_events() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::Anthropic);
        let response = a.transform_response(response("{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n"), true).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("event: content_block_delta"));
        assert!(text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn transform_response_aggregates_when_client_did_not_stream() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let output = a.transform_response(response("{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n"), false).await.unwrap();
        let body = axum::body::to_bytes(output.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert!(matches!(
            a.transform_response(response("{\"type\":\"error\",\"error\":\"boom\"}\n"), false)
                .await,
            Err(AppError::Upstream(_))
        ));
    }

    #[tokio::test]
    async fn needs_refresh_is_always_false() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        assert!(!a.needs_refresh(&creds()));
    }

    #[tokio::test]
    async fn refresh_credentials_echoes_the_credentials_back() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let c = creds();
        let got = a.refresh_credentials(&c).await.unwrap();
        assert_eq!(got.access_token, c.access_token);
        assert_eq!(got.api_key, c.api_key);
    }
}
