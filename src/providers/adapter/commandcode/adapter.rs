use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::{OnceLock, RwLock};
use uuid::Uuid;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::providers::adapter::codex::claude_bridge;
use crate::providers::adapter::commandcode::transform;
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const GENERATE_URL: &str = "https://api.commandcode.ai/alpha/generate";
pub const PROVIDER_CHAT_URL: &str = "https://api.commandcode.ai/provider/v1/chat/completions";
const PROVIDER_MESSAGES_URL: &str = "https://api.commandcode.ai/provider/v1/messages";
pub const DEFAULT_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";
const COMMAND_CODE_VERSION: &str = "0.29.0";

/// Which shape the upstream response for the in-flight request actually is,
/// as decided by `build_request`; consumed by `transform_response`. Distinct
/// from `Transport` (which only governs the Go-plan `/alpha/generate`
/// fallback and is read by `proxy::flow`/`providers::routes`) - a Claude-
/// family model under `Transport::Provider` still uses the provider
/// transport, it just speaks Anthropic Messages instead of OpenAI
/// chat-completions on the wire to Command Code. Keeping this as its own
/// field (rather than a third `Transport` variant) avoids breaking the
/// `current_transport(id) == Transport::Provider` guards the Go-plan
/// fallback relies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpstreamShape {
    OpenAiChat,
    AnthropicMessages,
    GenerateEnvelope,
}

/// Which upstream transport this provider uses. Command Code has two:
/// - `Provider`: the modern OpenAI-shaped `/provider/v1/chat/completions`
///   (what pi's `transport.ts` tries first)
/// - `Generate`: the legacy envelope-shaped `/alpha/generate`, required for
///   Go-plan accounts whose provider transport answers `403
///   {"error":{"code":"upgrade_required"}}`
/// Choice is remembered per provider id (like pi's in-process memory, reset
/// when the stored key changes) so the fallback only costs one extra request
/// per provider per process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Provider,
    Generate,
}

fn transport_cache() -> &'static DashMap<String, Transport> {
    static MAP: OnceLock<DashMap<String, Transport>> = OnceLock::new();
    MAP.get_or_init(DashMap::new)
}

pub fn current_transport(provider_id: &str) -> Transport {
    transport_cache()
        .get(provider_id)
        .map(|entry| *entry)
        .unwrap_or(Transport::Provider)
}

pub fn remember_transport(provider_id: &str, transport: Transport) {
    transport_cache().insert(provider_id.to_string(), transport);
}

pub fn reset_transport(provider_id: &str) {
    transport_cache().remove(provider_id);
}

/// The `upgrade_required` body pi's transport.ts keys on: a 403 from the
/// provider transport means this account must use `/alpha/generate`.
pub fn is_upgrade_required(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains("upgrade_required")
}

pub struct CommandCodeAdapter {
    provider: Provider,
    http: reqwest::Client,
    client_wire: WireFormat,
    /// Which transport the in-flight request used; set by `build_request`,
    /// consumed by `transform_response` (fresh adapter per request, so a
    /// `RwLock` is uncontended).
    transport: RwLock<Transport>,
    /// Which upstream wire shape the in-flight request actually used; see
    /// `UpstreamShape`.
    shape: RwLock<UpstreamShape>,
}

impl CommandCodeAdapter {
    pub fn new(provider: Provider, http: reqwest::Client, client_wire: WireFormat) -> Self {
        Self {
            provider,
            http,
            client_wire,
            transport: RwLock::new(Transport::Provider),
            shape: RwLock::new(UpstreamShape::OpenAiChat),
        }
    }

    fn shape(&self) -> UpstreamShape {
        self.shape
            .read()
            .map(|guard| *guard)
            .unwrap_or(UpstreamShape::OpenAiChat)
    }
}

fn generate_url() -> String {
    std::env::var("ROUTER_COMMANDCODE_BASE_URL")
        .map(|base| format!("{}/alpha/generate", base.trim_end_matches('/')))
        .unwrap_or_else(|_| GENERATE_URL.to_string())
}

fn provider_chat_url() -> String {
    std::env::var("ROUTER_COMMANDCODE_BASE_URL")
        .map(|base| format!("{}/provider/v1/chat/completions", base.trim_end_matches('/')))
        .unwrap_or_else(|_| PROVIDER_CHAT_URL.to_string())
}

fn provider_messages_url() -> String {
    std::env::var("ROUTER_COMMANDCODE_BASE_URL")
        .map(|base| format!("{}/provider/v1/messages", base.trim_end_matches('/')))
        .unwrap_or_else(|_| PROVIDER_MESSAGES_URL.to_string())
}

fn command_code_headers(builder: reqwest::RequestBuilder, cwd: &str) -> reqwest::RequestBuilder {
    builder
        .header("x-command-code-version", COMMAND_CODE_VERSION)
        .header("x-cli-environment", "production")
        .header(
            "x-project-slug",
            transform::project_slug_from_path(cwd),
        )
        .header("x-taste-learning", "true")
        .header("x-co-flag", "false")
}

#[async_trait::async_trait]
impl ProviderAdapter for CommandCodeAdapter {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError> {
        let raw_json: Value = serde_json::from_slice(client_body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cwd_string = cwd.to_string_lossy().to_string();
        let access = creds.access_token.as_ref().ok_or_else(|| {
            AppError::Internal("commandcode provider missing access_token".into())
        })?;

        // A stored generate-capable flag from the admin UI pins the transport;
        // otherwise use the in-process choice, defaulting to the provider
        // transport (pi's transport.ts tries it first).
        let provider_data = creds.provider_data.get("generate_only");
        let transport = match provider_data.and_then(Value::as_bool) {
            Some(true) => Transport::Generate,
            _ => current_transport(&self.provider.id),
        };
        if let Ok(mut guard) = self.transport.write() {
            *guard = transport;
        }

        let set_shape = |shape: UpstreamShape| {
            if let Ok(mut guard) = self.shape.write() {
                *guard = shape;
            }
        };

        if transport == Transport::Generate {
            // The generate envelope handles any model uniformly - Command
            // Code's Claude-family vs. everything-else split below is
            // specific to the modern `/provider/v1/*` API surface.
            let openai_json = match self.client_wire {
                WireFormat::Anthropic => claude_bridge::claude_to_openai_request(&raw_json),
                WireFormat::OpenAi => raw_json,
            };
            let mut envelope = transform::transform_request(
                &openai_json,
                &Uuid::new_v4().to_string(),
                &cwd_string,
            );
            envelope["params"]["model"] = Value::String(self.provider.upstream_model.clone());
            set_shape(UpstreamShape::GenerateEnvelope);
            return command_code_headers(
                self.http.post(generate_url()).json(&envelope).bearer_auth(access),
                &cwd_string,
            )
            .build()
            .map_err(|e| AppError::Internal(format!("commandcode request build failed: {e}")));
        }

        if transform::wants_messages_shape(&self.provider.upstream_model) {
            // Command Code's Provider API splits by model family: Claude
            // models 400 on `/provider/v1/chat/completions` and must go to
            // `/provider/v1/messages` (Anthropic Messages shape) instead. An
            // Anthropic-wire client's body already IS that shape - send it
            // close to as-is rather than round-tripping it through OpenAI
            // and back, which silently drops cache_control, top_k,
            // stop_sequences, thinking blocks, and URL-sourced images.
            let mut body = match self.client_wire {
                WireFormat::Anthropic => raw_json,
                WireFormat::OpenAi => transform::openai_to_commandcode_messages(&raw_json),
            };
            body["model"] = Value::String(self.provider.upstream_model.clone());
            set_shape(UpstreamShape::AnthropicMessages);
            return command_code_headers(
                self.http
                    .post(provider_messages_url())
                    .json(&body)
                    .bearer_auth(access),
                &cwd_string,
            )
            .build()
            .map_err(|e| AppError::Internal(format!("commandcode request build failed: {e}")));
        }

        // The provider transport is OpenAI-shaped, so the request body is the
        // translated client body itself (model rewritten, `stream` kept from
        // the client).
        let openai_json = match self.client_wire {
            WireFormat::Anthropic => claude_bridge::claude_to_openai_request(&raw_json),
            WireFormat::OpenAi => raw_json,
        };
        let mut provider_body = openai_json;
        provider_body["model"] = Value::String(self.provider.upstream_model.clone());
        set_shape(UpstreamShape::OpenAiChat);
        command_code_headers(
            self.http
                .post(provider_chat_url())
                .json(&provider_body)
                .bearer_auth(access),
            &cwd_string,
        )
        .build()
        .map_err(|e| AppError::Internal(format!("commandcode request build failed: {e}")))
    }

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();

        match self.shape() {
            // Provider transport, non-Claude model: upstream speaks OpenAI
            // chat.completions natively, so stream/JSON pass through with
            // only wire-format translation for Anthropic clients (same
            // shapes HttpAdapter handles).
            UpstreamShape::OpenAiChat => {
                return self.transform_provider_response(upstream, client_wanted_stream).await;
            }
            // Provider transport, Claude-family model: upstream speaks
            // Anthropic Messages natively.
            UpstreamShape::AnthropicMessages => {
                return self.transform_messages_response(upstream, client_wanted_stream).await;
            }
            UpstreamShape::GenerateEnvelope => {}
        }

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
            return Err(match transform::embedded_error_status(&error) {
                Some(status) => AppError::UpstreamWithStatus(status, error),
                None => AppError::Upstream(format!("commandcode embedded error: {error}")),
            });
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

impl CommandCodeAdapter {
    async fn transform_provider_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        if client_wanted_stream {
            // Upstream already emits OpenAI SSE; pass the raw stream through
            // for OpenAI clients, reframe+translate for Anthropic clients
            // (same path HttpAdapter uses).
            if self.client_wire == WireFormat::Anthropic {
                let framed = claude_bridge::reframe_sse_blocks(upstream.bytes_stream());
                let claude = claude_bridge::convert_openai_sse_to_claude_sse(framed);
                let mut response = (status, Body::from_stream(claude)).into_response();
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                return Ok(response);
            }
            let mut response = (status, Body::from_stream(upstream.bytes_stream())).into_response();
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            return Ok(response);
        }
        let body = upstream
            .text()
            .await
            .map_err(|e| AppError::Upstream(format!("commandcode response read: {e}")))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| AppError::Upstream(format!("commandcode invalid JSON: {e}")))?;
        if self.client_wire == WireFormat::Anthropic {
            return Ok((
                status,
                axum::Json(claude_bridge::openai_json_to_claude_message(&json)),
            )
                .into_response());
        }
        Ok((status, axum::Json(json)).into_response())
    }

    /// Response side of `Transport::Provider` + Claude-family model: upstream
    /// speaks Anthropic Messages natively (streaming or aggregated JSON). An
    /// Anthropic-wire client already wants that shape - pass it straight
    /// through (mirroring what the request side did) rather than
    /// translating; an OpenAI-wire client gets the same
    /// reframe-then-translate treatment `HttpAdapter` uses for Anthropic
    /// passthrough providers.
    async fn transform_messages_response(
        &self,
        upstream: reqwest::Response,
        client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        if client_wanted_stream {
            if self.client_wire == WireFormat::Anthropic {
                let mut response =
                    (status, Body::from_stream(upstream.bytes_stream())).into_response();
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                return Ok(response);
            }
            // Raw bytes from `bytes_stream()` don't align on `\n\n` block
            // boundaries; `convert_claude_sse_to_openai_sse` parses each
            // stream item as one complete block, so skipping this reframe
            // silently drops or truncates text instead of erroring.
            let framed = claude_bridge::reframe_sse_blocks(upstream.bytes_stream());
            let openai = claude_bridge::convert_claude_sse_to_openai_sse(framed);
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
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| AppError::Upstream(format!("commandcode invalid JSON: {e}")))?;
        if self.client_wire == WireFormat::Anthropic {
            return Ok((status, axum::Json(json)).into_response());
        }
        Ok((
            status,
            axum::Json(claude_bridge::claude_json_to_openai_message(&json)),
        )
            .into_response())
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
        prov_with_id("cc")
    }

    fn prov_with_id(id: &str) -> Provider {
        prov_with_model(id, "cc-model")
    }

    fn prov_with_model(id: &str, upstream_model: &str) -> Provider {
        Provider {
            id: id.into(),
            name: "Command Code".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::OauthCommandCode,
            base_url: None,
            api_key: None,
            upstream_model: upstream_model.into(),
            dataset_logging: false,
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
    fn creds_with_data(data: serde_json::Value) -> Credentials {
        Credentials {
            access_token: Some("cc-key-123".into()),
            provider_data: data,
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

    fn set_transport(a: &CommandCodeAdapter, transport: Transport) {
        if let Ok(mut guard) = a.transport.write() {
            *guard = transport;
        }
        // `transform_response` dispatches on `shape`, not `transport` -
        // mirror what `build_request` would have set for a non-Claude
        // model, since these tests only exercise that shape.
        if let Ok(mut guard) = a.shape.write() {
            *guard = match transport {
                Transport::Provider => UpstreamShape::OpenAiChat,
                Transport::Generate => UpstreamShape::GenerateEnvelope,
            };
        }
    }

    fn set_shape(a: &CommandCodeAdapter, shape: UpstreamShape) {
        if let Ok(mut guard) = a.shape.write() {
            *guard = shape;
        }
    }

    #[tokio::test]
    async fn build_request_defaults_to_provider_transport() {
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-provider-default"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[],"stream":true}))
                .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/provider/v1/chat/completions"
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
    async fn build_request_targets_generate_when_remembered() {
        remember_transport("cc-generate-remembered", Transport::Generate);
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-generate-remembered"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[],"stream":true}))
                .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/alpha/generate"
        );
    }

    #[tokio::test]
    async fn build_request_generate_only_provider_data_pins_generate_transport() {
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-pinned"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[]})).unwrap(),
        );
        let req = a
            .build_request(
                &body,
                &creds_with_data(serde_json::json!({"generate_only": true})),
            )
            .await
            .unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/alpha/generate"
        );
    }

    #[tokio::test]
    async fn provider_body_is_openai_shaped_with_rewritten_model() {
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-provider-body"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[]})).unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "cc-model");
        assert!(sent.get("params").is_none(), "provider body is not enveloped");
    }

    #[tokio::test]
    async fn build_request_rewrites_model_to_the_upstream_model() {
        remember_transport("cc-generate-model", Transport::Generate);
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-generate-model"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[]})).unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["params"]["model"], "cc-model");
    }

    #[tokio::test]
    async fn build_request_routes_claude_family_models_to_the_messages_endpoint() {
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-openai-client", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(
                &serde_json::json!({"model":"pool","messages":[{"role":"user","content":"hi"}]}),
            )
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/provider/v1/messages"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "claude-sonnet-5");
        assert_eq!(sent["messages"][0], serde_json::json!({"role":"user","content":"hi"}));
        // No client max_tokens - falls back to this provider's own default,
        // not claude_bridge's generic 4096.
        assert_eq!(sent["max_tokens"], transform::DEFAULT_MAX_TOKENS);
    }

    #[tokio::test]
    async fn build_request_sends_an_anthropic_clients_claude_body_near_untranslated() {
        // Regression: routing an Anthropic-wire client's request for a
        // Claude model through OpenAI-shaped intermediate translation and
        // back would silently drop fields OpenAI has no equivalent for
        // (top_k, stop_sequences, cache_control, ...). The only change from
        // the client's own body should be the rewritten `model`.
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-anthropic-client", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::Anthropic,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model":"pool",
                "max_tokens":64,
                "top_k":5,
                "stop_sequences":["STOP"],
                "messages":[{"role":"user","content":"hi"}]
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(
            req.url().as_str(),
            "https://api.commandcode.ai/provider/v1/messages"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "claude-sonnet-5");
        assert_eq!(sent["top_k"], 5);
        assert_eq!(sent["stop_sequences"], serde_json::json!(["STOP"]));
        assert_eq!(sent["max_tokens"], 64);
    }

    #[tokio::test]
    async fn build_request_merges_parallel_tool_results_into_one_user_message() {
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-tools", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model":"pool",
                "messages":[
                    {"role":"tool","tool_call_id":"a","content":"ok-a"},
                    {"role":"tool","tool_call_id":"b","content":"ok-b"}
                ]
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
        assert_eq!(sent["messages"][0]["role"], "user");
        assert_eq!(sent["messages"][0]["content"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn build_request_generate_transport_wins_over_claude_family_routing() {
        // Go-plan fallback must still work for Claude-family models: once
        // Generate is remembered, it takes priority over the
        // Claude-family-messages-endpoint branch.
        remember_transport("cc-claude-generate", Transport::Generate);
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-generate", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        let body =
            Bytes::from(serde_json::to_vec(&serde_json::json!({"model":"pool","messages":[]})).unwrap());
        let req = a.build_request(&body, &creds()).await.unwrap();
        assert_eq!(req.url().as_str(), "https://api.commandcode.ai/alpha/generate");
    }

    #[tokio::test]
    async fn transform_response_messages_shape_streaming_openai_wire_translates_claude_sse() {
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-resp", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        set_shape(&a, UpstreamShape::AnthropicMessages);
        let claude_sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-5\",\"usage\":{}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let response = a
            .transform_response(response(claude_sse), true)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("chat.completion.chunk"));
        assert!(text.contains("\"content\":\"hi\""));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn transform_response_messages_shape_streaming_anthropic_wire_passes_through() {
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-resp-anthropic", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::Anthropic,
        );
        set_shape(&a, UpstreamShape::AnthropicMessages);
        let claude_sse = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let response = a
            .transform_response(response(claude_sse), true)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(text, claude_sse);
    }

    #[tokio::test]
    async fn transform_response_messages_shape_non_streaming_translates_to_openai() {
        let a = CommandCodeAdapter::new(
            prov_with_model("cc-claude-resp-json", "claude-sonnet-5"),
            reqwest::Client::new(),
            WireFormat::OpenAi,
        );
        set_shape(&a, UpstreamShape::AnthropicMessages);
        let claude_json = serde_json::json!({
            "id":"msg_1","model":"claude-sonnet-5","stop_reason":"end_turn",
            "content":[{"type":"text","text":"hi"}],
            "usage":{"input_tokens":1,"output_tokens":1}
        })
        .to_string();
        let output = a
            .transform_response(response(&claude_json), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(output.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["content"], "hi");
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
        remember_transport("cc-generate-anthropic", Transport::Generate);
        let a = CommandCodeAdapter::new(
            prov_with_id("cc-generate-anthropic"),
            reqwest::Client::new(),
            WireFormat::Anthropic,
        );
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
        set_transport(&a, Transport::Generate);
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
        set_transport(&a, Transport::Generate);
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
        set_transport(&a, Transport::Generate);
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
    async fn provider_transport_passthroughs_openai_json() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        set_transport(&a, Transport::Provider);
        let openai = serde_json::json!({
            "id":"chatcmpl-1","object":"chat.completion","model":"cc-model",
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]
        })
        .to_string();
        let output = a
            .transform_response(response(&openai), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(output.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "cc-model");
        assert_eq!(json["choices"][0]["message"]["content"], "hi");
    }

    #[tokio::test]
    async fn provider_transport_passthroughs_openai_sse_for_openai_clients() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::OpenAi);
        set_transport(&a, Transport::Provider);
        let sse = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        let output = a
            .transform_response(response(sse), true)
            .await
            .unwrap();
        let body = axum::body::to_bytes(output.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.starts_with("data: {"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn provider_transport_translates_openai_json_to_claude_for_anthropic_clients() {
        let a = CommandCodeAdapter::new(prov(), reqwest::Client::new(), WireFormat::Anthropic);
        set_transport(&a, Transport::Provider);
        let openai = serde_json::json!({
            "id":"chatcmpl-1","object":"chat.completion","model":"cc-model",
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]
        })
        .to_string();
        let output = a
            .transform_response(response(&openai), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(output.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["content"][0]["text"], "hi");
    }

    #[test]
    fn upgrade_required_detection() {
        assert!(is_upgrade_required(
            StatusCode::FORBIDDEN,
            r#"{"error":{"code":"upgrade_required","message":"Provider API requires an upgrade"}}"#
        ));
        assert!(!is_upgrade_required(StatusCode::FORBIDDEN, r#"{"error":"forbidden"}"#));
        assert!(!is_upgrade_required(StatusCode::UNAUTHORIZED, "upgrade_required"));
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
