use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Utc;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::providers::adapter::codex::claude_bridge;
use crate::providers::adapter::codex::refresh;
use crate::providers::adapter::codex::transform;
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct CodexAdapter {
    provider: Provider,
    http: reqwest::Client,
    client_wire: WireFormat,
}

impl CodexAdapter {
    pub fn new(provider: Provider, http: reqwest::Client, client_wire: WireFormat) -> Self {
        Self { provider, http, client_wire }
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
        // A provider with wire_format = "anthropic" serves /v1/messages
        // clients (Claude Code) directly - bridge its Claude-shaped body into
        // the OpenAI Chat-Completions shape the rest of this pipeline speaks
        // before doing anything else.
        let client_json = match self.client_wire {
            WireFormat::Anthropic => claude_bridge::claude_to_openai_request(&client_json),
            WireFormat::OpenAi => client_json,
        };
        // prompt_cache_key hints which backend replica/cache scope Codex
        // routes a request to for prefix-cache locality. Scope it by pool
        // alias (self.provider.upstream_model is already the per-pool
        // model_override by the time build_request runs - see
        // proxy::flow::handle_proxy) and by client wire format, not just
        // provider id - otherwise every pool sharing this OAuth account
        // (and, since one Codex provider can now serve both wire formats,
        // every distinct client population) would interleave under one key,
        // diluting cache-hit locality even though no data ever crosses
        // between them.
        let wire_tag = match self.client_wire {
            WireFormat::OpenAi => "openai",
            WireFormat::Anthropic => "anthropic",
        };
        let session_id = format!(
            "1router-{}-{}-{}",
            self.provider.id, self.provider.upstream_model, wire_tag
        );
        let mut transformed = transform::transform_request(&client_json, &session_id);
        // The client's `model` is the pool id, not a real Codex model name -
        // rewrite to the provider's actual upstream model, matching
        // HttpAdapter's behavior (confirmed via a real-account 400:
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
        let is_anthropic = matches!(self.client_wire, WireFormat::Anthropic);
        if client_wanted_stream {
            // Responses-API SSE events (response.created, output_text.delta,
            // function_call_arguments.delta, ...) don't match what an
            // OpenAI-Chat-Completions-compatible client expects on the wire
            // (chat.completion.chunk / delta.content / delta.tool_calls) -
            // translate event-by-event instead of passing the body through.
            use futures::StreamExt;
            let stream = upstream.bytes_stream().boxed();
            let openai_chunks =
                transform::convert_sse_stream(stream, self.provider.upstream_model.clone());
            if is_anthropic {
                let claude_sse = claude_bridge::convert_openai_sse_to_claude_sse(openai_chunks);
                let mut response = (status, Body::from_stream(claude_sse)).into_response();
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                return Ok(response);
            }
            let mut response = (status, Body::from_stream(openai_chunks)).into_response();
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            return Ok(response);
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
        let json = transform::aggregate_sse(&text, &self.provider.upstream_model);
        if is_anthropic {
            let claude_json = claude_bridge::openai_json_to_claude_message(&json);
            return Ok((StatusCode::OK, axum::Json(claude_json)).into_response());
        }
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
            dataset_logging: false,
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
        let a = CodexAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
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

    #[tokio::test]
    async fn prompt_cache_key_is_scoped_by_pool_alias_and_client_wire() {
        let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"messages": []})).unwrap());

        let mut openai_provider = prov();
        openai_provider.upstream_model = "gpt-5.6-sol".into();
        let a = CodexAdapter::new(openai_provider, reqwest::Client::new(), WireFormat::OpenAi);
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["prompt_cache_key"], "1router-cx-gpt-5.6-sol-openai");

        // Same provider id, same pool alias, but the Anthropic-facing wire
        // format - must get a distinct key so Claude Code's and OpenAI-SDK
        // clients' traffic don't interleave under one cache-routing hint.
        let mut anthropic_provider = prov();
        anthropic_provider.upstream_model = "gpt-5.6-sol".into();
        let b = CodexAdapter::new(anthropic_provider, reqwest::Client::new(), WireFormat::Anthropic);
        let req2 = b.build_request(&body, &creds()).await.unwrap();
        let sent2: serde_json::Value =
            serde_json::from_slice(req2.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent2["prompt_cache_key"], "1router-cx-gpt-5.6-sol-anthropic");
    }

    #[tokio::test]
    async fn build_request_bridges_anthropic_wire_format_to_responses_api() {
        let mut provider = prov();
        provider.wire_format = WireFormat::OpenAi;
        let a = CodexAdapter::new(provider, reqwest::Client::new(), WireFormat::Anthropic);
        // A Claude Code /v1/messages request: system + a text message.
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o",
                "system": "be concise",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert_eq!(
            req.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        // Claude's `system`/`messages` were bridged into the Responses API's
        // `input` shape, same as an OpenAI-shaped request would be.
        assert!(sent.get("messages").is_none());
        assert_eq!(sent["input"][0]["role"], "developer");
        assert_eq!(sent["input"][0]["content"][0]["text"], "be concise");
        assert_eq!(sent["input"][1]["role"], "user");
        assert_eq!(sent["input"][1]["content"][0]["text"], "hi");
        assert_eq!(sent["model"], "gpt-5-codex");
    }

    #[tokio::test]
    async fn transform_response_bridges_to_anthropic_wire_format_independent_of_provider_wire() {
        let provider = prov();
        let a = CodexAdapter::new(provider, reqwest::Client::new(), WireFormat::Anthropic);
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
        );
        let upstream = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(Bytes::from(sse))
                .unwrap(),
        );

        let response = a.transform_response(upstream, false).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["content"][0]["text"], "hello");
        assert_eq!(json["stop_reason"], "end_turn");
        assert_eq!(json["usage"]["input_tokens"], 2);
        assert_eq!(json["usage"]["output_tokens"], 1);
        // Regression: aggregate_sse used to omit `model` entirely, so the
        // non-streaming Claude response always reported "unknown" even
        // though the streaming path correctly reported the real model.
        assert_eq!(json["model"], "gpt-5-codex");
    }
}
