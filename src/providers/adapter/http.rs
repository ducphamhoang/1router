use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::core::reasoning;
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
        // Keep the original, pre-translation body around: the
        // client-already-chose-an-effort check has to run against it (see
        // below), and `claude_bridge` translation consumes/rewrites fields.
        let client_json_for_override_check = client_json.clone();
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
        // Reasoning-effort default injection. Note `client_json` (the
        // pre-translation body) is what gets checked for an explicit client
        // choice - translation can drop the very fields that check needs -
        // while `json` (the post-translation, upstream-shaped body) is what
        // gets mutated. `effort_to_inject` also re-runs `capability_for`
        // here, which is the real guarantee: a stale value that reached this
        // point through config import / the onboarding wizard / direct
        // `<provider_id>/<model>` addressing is skipped silently rather than
        // turning a working request into a 400.
        if let Some((capability, level)) = reasoning::effort_to_inject(
            self.provider.kind,
            self.provider.wire_format,
            &self.provider.upstream_model,
            self.provider.default_reasoning_effort,
            &client_json_for_override_check,
        ) {
            match capability {
                reasoning::ReasoningCapability::OpenAiEffort => {
                    reasoning::inject_openai_effort(&mut json, level)
                }
                reasoning::ReasoningCapability::AnthropicThinkingBudget => {
                    reasoning::inject_anthropic_thinking(&mut json, level)
                }
                reasoning::ReasoningCapability::Unsupported => {}
            }
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
            default_reasoning_effort: None,
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

    // ---- reasoning-effort default injection -------------------------------

    use crate::core::model::EffortLevel;

    fn gpt5_provider(effort: Option<EffortLevel>) -> Provider {
        let mut p = prov();
        p.upstream_model = "gpt-5.1".into();
        p.default_reasoning_effort = effort;
        p
    }

    fn claude_provider(effort: Option<EffortLevel>) -> Provider {
        let mut p = prov();
        p.wire_format = WireFormat::Anthropic;
        p.base_url = Some("https://api.anthropic.com/v1/messages".into());
        p.upstream_model = "claude-sonnet-4-5".into();
        p.default_reasoning_effort = effort;
        p
    }

    async fn sent_body(a: &HttpAdapter, body: serde_json::Value) -> serde_json::Value {
        let bytes = Bytes::from(serde_json::to_vec(&body).unwrap());
        let req = a.build_request(&bytes, &creds()).await.unwrap();
        serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn injects_reasoning_effort_for_an_openai_wire_gpt5_provider() {
        let a = HttpAdapter::new(
            gpt5_provider(Some(EffortLevel::High)),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let sent = sent_body(&a, serde_json::json!({"model": "pool", "messages": []})).await;
        assert_eq!(sent["reasoning_effort"], "high");
        assert!(sent.get("thinking").is_none());
    }

    #[tokio::test]
    async fn injects_thinking_for_an_anthropic_wire_claude_provider() {
        let a = HttpAdapter::new(
            claude_provider(Some(EffortLevel::Medium)),
            reqwest::Client::new(),
            WireFormat::Anthropic,
        );
        let sent = sent_body(
            &a,
            serde_json::json!({"model": "pool", "max_tokens": 512, "temperature": 0.7, "messages": []}),
        )
        .await;
        let budget = crate::core::reasoning::ReasoningCapability::anthropic_budget_tokens(
            EffortLevel::Medium,
        ) as i64;
        assert_eq!(sent["thinking"]["type"], "enabled");
        assert_eq!(sent["thinking"]["budget_tokens"], budget);
        // normalization: max_tokens raised above budget, sampling params stripped
        assert_eq!(sent["max_tokens"].as_i64().unwrap(), budget + 1024);
        assert!(sent.get("temperature").is_none());
        assert!(sent.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn normalizes_max_tokens_on_the_translated_openai_to_claude_path_too() {
        // claude_bridge defaults max_tokens to 4096 and copies temperature
        // straight through - both would make an injected `thinking` invalid.
        let a = HttpAdapter::new(
            claude_provider(Some(EffortLevel::High)),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let sent = sent_body(
            &a,
            serde_json::json!({
                "model": "pool", "temperature": 0.5,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
        let budget = crate::core::reasoning::ReasoningCapability::anthropic_budget_tokens(
            EffortLevel::High,
        ) as i64;
        assert_eq!(sent["thinking"]["budget_tokens"], budget);
        assert_eq!(sent["max_tokens"].as_i64().unwrap(), budget + 1024);
        assert!(sent.get("temperature").is_none());
    }

    #[tokio::test]
    async fn does_not_inject_when_the_client_already_chose_an_effort() {
        for client_field in [
            serde_json::json!({"reasoning_effort": "low"}),
            serde_json::json!({"reasoning": {"effort": "low"}}),
            serde_json::json!({"thinking": {"type": "disabled"}}),
        ] {
            let a = HttpAdapter::new(
                gpt5_provider(Some(EffortLevel::High)),
                reqwest::Client::new(),
                WireFormat::OpenAi,
            );
            let mut body = serde_json::json!({"model": "pool", "messages": []});
            for (k, v) in client_field.as_object().unwrap() {
                body[k] = v.clone();
            }
            let sent = sent_body(&a, body).await;
            assert_ne!(
                sent["reasoning_effort"], "high",
                "provider default must not override the client's own choice ({client_field})"
            );
        }
    }

    #[tokio::test]
    async fn does_not_inject_when_no_default_is_configured() {
        let a = HttpAdapter::new(
            gpt5_provider(None),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let sent = sent_body(&a, serde_json::json!({"model": "pool", "messages": []})).await;
        assert!(sent.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn request_time_defense_silently_skips_an_unsupported_stale_config() {
        // A stale/imported `default_reasoning_effort` on a model that can't
        // carry one must degrade to "inject nothing" - never to an error,
        // and never to an invalid upstream body.
        let mut p = prov(); // openai wire, upstream_model "real-model"
        p.default_reasoning_effort = Some(EffortLevel::High);
        let a = HttpAdapter::new(p, reqwest::Client::new(), WireFormat::OpenAi);
        let sent = sent_body(&a, serde_json::json!({"model": "pool", "messages": []})).await;
        assert!(sent.get("reasoning_effort").is_none());
        assert!(sent.get("thinking").is_none());

        // ...and specifically: a Claude-named model behind an OpenAI-wire
        // mirror never gets a `thinking` object in an OpenAI-shaped body.
        let mut claude_named_openai_mirror = prov();
        claude_named_openai_mirror.upstream_model = "claude-sonnet-4-5".into();
        claude_named_openai_mirror.default_reasoning_effort = Some(EffortLevel::High);
        let a = HttpAdapter::new(
            claude_named_openai_mirror,
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let sent = sent_body(&a, serde_json::json!({"model": "pool", "messages": []})).await;
        assert!(sent.get("thinking").is_none());
        assert!(sent.get("reasoning_effort").is_none());
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
