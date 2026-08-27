use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::providers::adapter::codex::claude_bridge;
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Adapter for `ProviderKind::Passthrough` (config-only OpenAI/Anthropic
/// providers, as opposed to the OAuth-based Codex/Command Code kinds).
/// Despite the kind's name, this only passes bytes through untouched when
/// `client_wire == provider.wire_format` (see `translates()`); otherwise it
/// runs full bidirectional wire-format translation via `claude_bridge`.
pub struct HttpAdapter {
    provider: Provider,
    http: reqwest::Client,
    client_wire: WireFormat,
}

impl HttpAdapter {
    pub fn new(provider: Provider, http: reqwest::Client, client_wire: WireFormat) -> Self {
        Self {
            provider,
            http,
            client_wire,
        }
    }

    fn translates(&self) -> bool {
        self.client_wire != self.provider.wire_format
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for HttpAdapter {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError> {
        let client_json: serde_json::Value = serde_json::from_slice(client_body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;
        let mut json = if self.translates() {
            match self.client_wire {
                WireFormat::Anthropic => claude_bridge::claude_to_openai_request(&client_json),
                WireFormat::OpenAi => claude_bridge::openai_to_claude_request(&client_json),
            }
        } else {
            client_json
        };
        if let Some(obj) = json.as_object_mut() {
            obj.insert(
                "model".into(),
                serde_json::Value::String(self.provider.upstream_model.clone()),
            );
        }
        let url = self
            .provider
            .base_url
            .clone()
            .ok_or_else(|| AppError::Internal("passthrough provider missing base_url".into()))?;

        let mut builder = self.http.post(url).json(&json);
        if let Some(key) = creds.api_key.as_ref() {
            builder = match self.provider.wire_format {
                WireFormat::OpenAi => builder.bearer_auth(key),
                WireFormat::Anthropic => builder
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION),
            };
        }
        builder
            .build()
            .map_err(|e| AppError::Internal(format!("request build failed: {e}")))
    }

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        let mut resp_headers = HeaderMap::new();
        for (k, v) in upstream.headers().iter() {
            if k.as_str().eq_ignore_ascii_case("transfer-encoding")
                || k.as_str().eq_ignore_ascii_case("content-length")
            {
                continue;
            }
            resp_headers.insert(k.clone(), v.clone());
        }

        if !self.translates() {
            let body = Body::from_stream(upstream.bytes_stream());
            let mut response = (status, body).into_response();
            *response.headers_mut() = resp_headers;
            return Ok(response);
        }

        if client_wanted_stream {
            let framed = claude_bridge::reframe_sse_blocks(upstream.bytes_stream());
            let body = match self.client_wire {
                WireFormat::Anthropic => {
                    Body::from_stream(claude_bridge::convert_openai_sse_to_claude_sse(framed))
                }
                WireFormat::OpenAi => {
                    Body::from_stream(claude_bridge::convert_claude_sse_to_openai_sse(framed))
                }
            };
            let mut response = (status, body).into_response();
            *response.headers_mut() = resp_headers;
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            return Ok(response);
        }

        let bytes = upstream
            .bytes()
            .await
            .map_err(|e| AppError::Internal(format!("failed to read upstream body: {e}")))?;
        let upstream_json: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| AppError::Internal(format!("invalid upstream JSON: {e}")))?;
        let translated = match self.client_wire {
            WireFormat::Anthropic => claude_bridge::openai_json_to_claude_message(&upstream_json),
            WireFormat::OpenAi => claude_bridge::claude_json_to_openai_message(&upstream_json),
        };
        let mut response = (status, axum::Json(translated)).into_response();
        *response.headers_mut() = resp_headers;
        response
            .headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        Ok(response)
    }

    async fn classify_error(&self, status: StatusCode, headers: &HeaderMap) -> ErrorClass {
        backoff::classify(status, headers)
    }

    fn needs_refresh(&self, _creds: &Credentials) -> bool {
        false
    }

    async fn refresh_credentials(&self, _creds: &Credentials) -> Result<Credentials, RefreshError> {
        Err(RefreshError::Transient(
            "passthrough has no refresh".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use crate::providers::adapter::Credentials;
    use bytes::Bytes;
    use chrono::Utc;

    fn prov() -> Provider {
        Provider {
            id: "p1".into(),
            name: "P1".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough,
            base_url: Some("https://api.example.com/v1/chat/completions".into()),
            api_key: Some("sk-xyz".into()),
            upstream_model: "real-model".into(),
            dataset_logging: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn creds() -> Credentials {
        Credentials {
            api_key: Some("sk-xyz".into()),
            access_token: None,
            refresh_token: None,
            id_token: None,
            access_expires_at: None,
            provider_data: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn build_request_rewrites_model_and_sets_auth() {
        let a = HttpAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o", "messages": []
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert_eq!(
            req.headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer sk-xyz"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "real-model");
    }

    #[tokio::test]
    async fn build_request_uses_anthropic_headers_for_anthropic_wire_format() {
        let mut p = prov();
        p.wire_format = WireFormat::Anthropic;
        let a = HttpAdapter::new(p, reqwest::Client::new(), WireFormat::Anthropic);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({ "model": "claude", "messages": [] })).unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert!(req.headers().get("authorization").is_none());
        assert_eq!(
            req.headers().get("x-api-key").unwrap().to_str().unwrap(),
            "sk-xyz"
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .unwrap()
                .to_str()
                .unwrap(),
            ANTHROPIC_VERSION
        );
    }

    #[test]
    fn needs_refresh_is_false() {
        let a = HttpAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        assert!(!a.needs_refresh(&creds()));
    }

    #[tokio::test]
    async fn build_request_translates_anthropic_client_to_openai_provider() {
        let a = HttpAdapter::new(prov(), reqwest::Client::new(), WireFormat::Anthropic);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-x", "system": "be nice", "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "real-model");
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][0]["content"], "be nice");
        assert_eq!(sent["messages"][1]["content"], "hi");
    }

    #[tokio::test]
    async fn build_request_translates_openai_client_to_anthropic_provider() {
        let mut p = prov();
        p.wire_format = WireFormat::Anthropic;
        let a = HttpAdapter::new(p, reqwest::Client::new(), WireFormat::OpenAi);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "real-model");
        assert_eq!(sent["messages"][0]["content"], "hi");
        assert!(sent.get("max_tokens").is_some(), "Anthropic requires max_tokens");
    }

    fn upstream_response(body: &str) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(body.to_string())
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn transform_response_translates_openai_json_to_claude_when_client_is_anthropic() {
        let a = HttpAdapter::new(prov(), reqwest::Client::new(), WireFormat::Anthropic);
        let openai_json = serde_json::json!({
            "id": "resp_1", "model": "real-model",
            "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}]
        })
        .to_string();
        let response = a
            .transform_response(upstream_response(&openai_json), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let out: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn transform_response_translates_claude_json_to_openai_when_client_is_openai() {
        let mut p = prov();
        p.wire_format = WireFormat::Anthropic;
        let a = HttpAdapter::new(p, reqwest::Client::new(), WireFormat::OpenAi);
        let claude_json = serde_json::json!({
            "id": "msg_1", "model": "real-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello"}]
        })
        .to_string();
        let response = a
            .transform_response(upstream_response(&claude_json), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let out: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
    }
}
