//! `POST /v1/images/generations` and `POST /v1/images/edits` (OpenAI
//! Images API, `b64_json` only), served by image pools of Codex OAuth
//! providers. Edits carry reference images (multipart files or JSON data
//! URLs); upstream has no mask support. Buffered, not streamed:
//! the Codex SSE body is read whole (bounded by
//! `MediaConfig.max_response_bytes`) and classified by
//! `codex_images::parse_stream`.
//!
//! Failover rules (plan §4): anything upstream says *before* generating
//! (non-2xx status, usage limit, no image entitlement) moves to the next
//! member; anything about *this request* (refusal, moderation, a bad
//! parameter) or a generation that was cut short is returned as-is, so one
//! prompt never burns quota on several accounts.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{header::CONTENT_TYPE, HeaderValue, StatusCode};
use base64::Engine as _;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::core::error::RefreshError;
use crate::core::http_client::{read_body_limited, read_text_truncated, MAX_ERROR_BODY};
use crate::core::model::{LogEntry, Modality, Provider};
use crate::core::runtime::runtime_key;
use crate::core::state::AppState;
use crate::media::codex_images::{
    build_body, build_request, classify_status, error_message, parse_stream, FailureKind,
    ImageParams, PreStream, StreamOutcome,
};
use crate::media::MediaState;
use crate::pools::select::select_image;
use crate::providers::adapter::{adapter_for, Credentials, ProviderAdapter};
use crate::providers::refresh_lock::refresh_and_persist_detached;
use crate::proxy::backoff;
use crate::proxy::body::buffer_body;
use crate::proxy::flow::credentials_for;
use crate::users::Caller;

pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Largest `/images/edits` body: the reference images ride in it.
pub const MAX_EDIT_BODY_BYTES: usize = 50 * 1024 * 1024;
/// Most reference images per edit (OpenAI's own limit).
pub const MAX_REFERENCE_IMAGES: usize = 16;
/// Largest single reference image, decoded.
pub const MAX_REFERENCE_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/images/generations", post(generations))
        .route("/v1/images/edits", post(edits).layer(DefaultBodyLimit::max(MAX_EDIT_BODY_BYTES)))
}

/// OpenAI-shaped error body.
fn image_error(status: StatusCode, code: &str, message: &str) -> Response {
    let kind = if status.is_client_error() { "invalid_request_error" } else { "upstream_error" };
    (
        status,
        Json(json!({ "error": { "message": message, "type": kind, "code": code, "param": null } })),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    image_error(StatusCode::BAD_REQUEST, "invalid_request", message)
}

// `Caller` is inserted by `auth::middleware::require_bearer`; `Option` so a
// router built without that layer still works, as anonymous.
#[allow(clippy::result_large_err)]
fn admit(state: &AppState, caller: Option<Extension<Caller>>) -> Result<Caller, Response> {
    if !state.media.images_enabled() {
        return Err(image_error(StatusCode::NOT_FOUND, "not_found", "image generation is not enabled"));
    }
    let caller = caller.map(|Extension(c)| c).unwrap_or_default();
    // Images spend subscription quota fast: never for anonymous open access.
    if caller.user_id.is_none() {
        return Err(image_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "image generation requires an API key",
        ));
    }
    Ok(caller)
}

async fn generations(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    body: Body,
) -> Response {
    let caller = match admit(&state, caller) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let bytes = match buffer_body(body, state.config.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let request = match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => v,
        Err(_) => return bad_request("request body must be JSON"),
    };
    let (model, params) = match parse_request(&request) {
        Ok(p) => p,
        Err(msg) => return bad_request(&msg),
    };
    respond(&state, &caller, &model, &params).await
}

/// `multipart/form-data` (what the OpenAI SDKs send: `image` / `image[]`
/// files) or JSON with `images: [{"image_url": "data:..."}]`.
async fn edits(State(state): State<AppState>, caller: Option<Extension<Caller>>, request: Request) -> Response {
    let caller = match admit(&state, caller) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let multipart = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("multipart/form-data"));
    let parsed = if multipart {
        match Multipart::from_request(request, &state).await {
            Ok(form) => read_multipart(form).await,
            Err(e) => return image_error(e.status(), "invalid_request", &e.body_text()),
        }
    } else {
        let bytes = match buffer_body(request.into_body(), MAX_EDIT_BODY_BYTES).await {
            Ok(b) => b,
            Err(e) => return e.into_response(),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => parse_edit_json(&v).map_err(|m| bad_request(&m)),
            Err(_) => Err(bad_request("request body must be multipart/form-data or JSON")),
        }
    };
    match parsed {
        Ok((model, params)) => respond(&state, &caller, &model, &params).await,
        Err(resp) => resp,
    }
}

fn multipart_error(e: axum::extract::multipart::MultipartError) -> Response {
    image_error(e.status(), "invalid_request", &e.body_text())
}

#[allow(clippy::result_large_err)]
async fn read_multipart(mut form: Multipart) -> Result<(String, ImageParams), Response> {
    let mut fields = serde_json::Map::new();
    let mut images = Vec::new();
    while let Some(field) = form.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "image" | "image[]" => {
                if images.len() == MAX_REFERENCE_IMAGES {
                    return Err(bad_request(&format!("at most {MAX_REFERENCE_IMAGES} images are supported")));
                }
                let bytes = field.bytes().await.map_err(multipart_error)?;
                images.push(image_data_url(&bytes).map_err(|m| bad_request(&m))?);
            }
            "mask" => return Err(bad_request("'mask' is not supported")),
            _ => {
                let text = field.text().await.map_err(multipart_error)?;
                // Form fields are strings; `parse_request` expects JSON types.
                let value = match name.as_str() {
                    "n" => text.trim().parse::<u64>().map(Value::from).unwrap_or(Value::String(text)),
                    "stream" => text.trim().parse::<bool>().map(Value::from).unwrap_or(Value::String(text)),
                    _ => Value::String(text),
                };
                fields.insert(name, value);
            }
        }
    }
    let (model, mut params) = parse_request(&Value::Object(fields)).map_err(|m| bad_request(&m))?;
    if images.is_empty() {
        return Err(bad_request("missing 'image' file"));
    }
    params.images = images;
    Ok((model, params))
}

/// JSON edits: `images` as `[{"image_url": ...}]` or plain strings, and/or
/// `image` as a string or an array. Data URLs or bare base64 only - no
/// remote URLs or file ids.
pub fn parse_edit_json(v: &Value) -> Result<(String, ImageParams), String> {
    if !v["mask"].is_null() {
        return Err("'mask' is not supported".into());
    }
    let (model, mut params) = parse_request(v)?;
    let mut refs = Vec::new();
    for key in ["images", "image"] {
        match &v[key] {
            Value::Null => {}
            Value::Array(items) => refs.extend(items.iter()),
            other => refs.push(other),
        }
    }
    if refs.is_empty() {
        return Err("missing 'images' (reference images)".into());
    }
    if refs.len() > MAX_REFERENCE_IMAGES {
        return Err(format!("at most {MAX_REFERENCE_IMAGES} images are supported"));
    }
    for r in refs {
        let s = r.as_str().or_else(|| r["image_url"].as_str()).or_else(|| r["image_url"]["url"].as_str());
        let s = s.ok_or("each image must be a data URL string or {\"image_url\": \"data:...\"}")?;
        let payload = match s.strip_prefix("data:") {
            Some(rest) => rest.split_once(";base64,").map(|(_, b)| b).ok_or("image data URLs must be base64")?,
            None if s.starts_with("http://") || s.starts_with("https://") => {
                return Err("remote image URLs are not supported; send the image as a data URL".into())
            }
            None => s,
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|_| "an image is not valid base64".to_string())?;
        params.images.push(image_data_url(&bytes)?);
    }
    Ok((model, params))
}

/// Sniffs the format (never the client's content type) and re-encodes the
/// image as a data URL.
fn image_data_url(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_REFERENCE_IMAGE_BYTES {
        return Err(format!("an image exceeds {} MiB", MAX_REFERENCE_IMAGE_BYTES / (1024 * 1024)));
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err("images must be PNG, JPEG or WebP".into());
    };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

async fn respond(state: &AppState, caller: &Caller, model: &str, params: &ImageParams) -> Response {
    let mut tried = Vec::new();
    let mut resp = generate(state, caller, model, params, &mut tried).await;
    // Routing topology is the admin's business only (SEC-11).
    if caller.is_admin() && !tried.is_empty() {
        let headers = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&tried.join(",")) {
            headers.insert("x-1router-tried", v);
        }
        if let Ok(v) = HeaderValue::from_str(tried.last().unwrap()) {
            headers.insert("x-1router-provider", v);
        }
    }
    resp
}

fn opt_enum(v: &Value, key: &str, allowed: &[&str]) -> Result<Option<String>, String> {
    match &v[key] {
        Value::Null => Ok(None),
        Value::String(s) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
        _ => Err(format!("'{key}' must be one of: {}", allowed.join(", "))),
    }
}

fn valid_size(s: &str) -> bool {
    let dim = |d: &str| (3..=4).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit());
    s == "auto" || s.split_once('x').is_some_and(|(w, h)| dim(w) && dim(h))
}

/// Validate a client request (plan §2). Unknown fields (`user`, ...) are
/// ignored, as OpenAI-compatible servers usually do.
pub fn parse_request(v: &Value) -> Result<(String, ImageParams), String> {
    let model = v["model"].as_str().filter(|m| !m.is_empty()).ok_or("missing 'model' field")?;
    let prompt = v["prompt"].as_str().map(str::trim).filter(|p| !p.is_empty()).ok_or("missing 'prompt' field")?;
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!("'prompt' exceeds {} KiB", MAX_PROMPT_BYTES / 1024));
    }
    match &v["n"] {
        Value::Null => {}
        n if n.as_u64() == Some(1) => {}
        _ => return Err("only n=1 is supported".into()),
    }
    match &v["response_format"] {
        Value::Null => {}
        Value::String(f) if f == "b64_json" => {}
        _ => return Err("only response_format=b64_json is supported".into()),
    }
    if v["stream"].as_bool() == Some(true) {
        return Err("streaming image generation is not supported".into());
    }
    let size = match &v["size"] {
        Value::Null => None,
        Value::String(s) if valid_size(s) => Some(s.clone()),
        _ => return Err("'size' must be 'auto' or WIDTHxHEIGHT (e.g. 1024x1024)".into()),
    };
    let params = ImageParams {
        prompt: prompt.to_string(),
        size,
        quality: opt_enum(v, "quality", &["low", "medium", "high", "auto"])?,
        background: opt_enum(v, "background", &["transparent", "opaque", "auto"])?,
        output_format: opt_enum(v, "output_format", &["png", "jpeg", "webp"])?.unwrap_or_else(|| "png".into()),
        images: Vec::new(),
    };
    Ok((model.to_string(), params))
}

/// Logs one member attempt exactly once: explicitly via `finish`, or - if
/// the handler future is dropped mid-generation (client disconnect) - as a
/// failure on drop, so a quota-spending call never goes unrecorded.
struct AttemptLog {
    tx: mpsc::Sender<LogEntry>,
    entry: Option<LogEntry>,
    start: Instant,
}

impl AttemptLog {
    fn new(state: &AppState, caller: &Caller, pool_id: &str, provider_id: &str) -> AttemptLog {
        AttemptLog {
            tx: state.log_tx.clone(),
            entry: Some(LogEntry {
                pool_id: Some(pool_id.to_string()),
                provider_id: Some(provider_id.to_string()),
                status_code: None,
                latency_ms: 0,
                success: false,
                user_id: caller.user_id.clone(),
                modality: Some(Modality::Image),
                units: None,
            }),
            start: Instant::now(),
        }
    }

    fn finish(mut self, status: Option<u16>, success: bool) {
        if let Some(mut e) = self.entry.take() {
            e.status_code = status.map(i64::from);
            e.success = success;
            e.units = success.then_some(1.0);
            e.latency_ms = self.start.elapsed().as_millis() as i64;
            // Logging must never block the hot path.
            let _ = self.tx.try_send(e);
        }
    }
}

impl Drop for AttemptLog {
    fn drop(&mut self) {
        if let Some(mut e) = self.entry.take() {
            e.latency_ms = self.start.elapsed().as_millis() as i64;
            let _ = self.tx.try_send(e);
        }
    }
}

enum Attempt {
    /// 2xx: the whole SSE body.
    Sse(String),
    /// Non-2xx before any generation.
    Status { status: u16, retry_after: Option<Duration>, body: String },
    BuildFailed(String),
    SendFailed(String),
    /// The body broke off or exceeded the response cap.
    ReadFailed(String),
    TimedOut,
}

async fn attempt(media: &MediaState, creds: &Credentials, body: &Value) -> Attempt {
    let req = match build_request(&media.http, &media.config.codex_responses_url, creds, body) {
        Ok(r) => r,
        Err(e) => return Attempt::BuildFailed(e),
    };
    let call = async {
        let resp = match media.http.execute(req).await {
            Ok(r) => r,
            Err(e) => return Attempt::SendFailed(e.without_url().to_string()),
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = backoff::reset_after_from_header(resp.headers());
            let body = read_text_truncated(resp, MAX_ERROR_BODY).await;
            return Attempt::Status { status: status.as_u16(), retry_after, body };
        }
        match read_body_limited(resp, media.config.max_response_bytes).await {
            Ok(b) => Attempt::Sse(String::from_utf8_lossy(&b).into_owned()),
            Err(e) => Attempt::ReadFailed(e),
        }
    };
    tokio::time::timeout(media.config.request_timeout, call).await.unwrap_or(Attempt::TimedOut)
}

fn success_body(img: crate::media::codex_images::GeneratedImage) -> Value {
    let mut item = json!({ "b64_json": img.b64_json });
    if let Some(rp) = img.revised_prompt {
        item["revised_prompt"] = json!(rp);
    }
    // Non-standard: text upstream sent next to the image (e.g. why the
    // prompt was changed). SDKs ignore unknown fields.
    if let Some(t) = img.text {
        item["text"] = json!(t);
    }
    let mut out = json!({ "created": chrono::Utc::now().timestamp(), "data": [item] });
    for (key, value) in [
        ("size", img.size),
        ("quality", img.quality),
        ("background", img.background),
        ("output_format", img.output_format),
    ] {
        if let Some(v) = value {
            out[key] = json!(v);
        }
    }
    if let Some(u) = img.usage {
        out["usage"] = u;
    }
    out
}

async fn generate(
    state: &AppState,
    caller: &Caller,
    model: &str,
    params: &ImageParams,
    tried: &mut Vec<String>,
) -> Response {
    let snapshot = state.snapshot.load();
    let Some(selection) = select_image(&snapshot, model, &state.pool_rotation) else {
        return bad_request(&format!("unknown image model or pool '{model}'"));
    };
    if selection.pool.is_none() && !caller.is_admin() {
        return bad_request(&format!("direct addressing of image model '{model}' is admin-only"));
    }
    // Cheap checks first; the permit bounds memory (each call can buffer up
    // to `max_response_bytes`) and upstream parallelism.
    let Ok(_permit) = state.media.permits.try_acquire() else {
        return image_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "too many image generations in flight, retry shortly",
        );
    };

    let mark = |provider: &Provider, effective_model: &str, f: &dyn Fn(&mut crate::core::runtime::ProviderRuntimeState)| {
        let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
        f(&mut st);
    };
    let backoff_retry = |st: &mut crate::core::runtime::ProviderRuntimeState| {
        let cooldown = backoff::cooldown_for(st.backoff_level + 1);
        st.record_retryable(cooldown, Instant::now());
    };

    let mut last_error = String::from("no image provider available");
    for member in &selection.providers {
        let provider = member.provider;
        let effective_model = member.effective_model.as_str();
        // `get`, not `entry`: never create state just by looking (SEC-03).
        if let Some(st) = state.runtime.get(&runtime_key(&provider.id, effective_model)) {
            if !st.is_available(Instant::now()) {
                continue;
            }
        }
        tried.push(provider.id.clone());
        let log = AttemptLog::new(state, caller, model, &provider.id);
        let body = build_body(effective_model, &state.media.config.codex_image_host_model, params);
        let creds = credentials_for(state, provider).await;
        let mut result = attempt(&state.media, &creds, &body).await;

        if matches!(result, Attempt::Status { status: 401, .. }) {
            if creds.refresh_token.is_none() {
                mark(provider, effective_model, &|st| st.mark_misconfigured(Instant::now()));
                log.finish(Some(401), false);
                last_error = format!("provider '{}' rejected its credentials", provider.id);
                continue;
            }
            let adapter: std::sync::Arc<dyn ProviderAdapter> = adapter_for(provider, state.http.clone()).into();
            // Detached: a client disconnect mustn't lose the rotated token.
            match refresh_and_persist_detached(state, provider, adapter, &creds).await {
                Ok(new_creds) => result = attempt(&state.media, &new_creds, &body).await,
                Err(e) => {
                    match e {
                        RefreshError::InvalidGrant => {
                            mark(provider, effective_model, &|st| st.mark_misconfigured(Instant::now()))
                        }
                        RefreshError::Transient(_) => mark(provider, effective_model, &backoff_retry),
                    }
                    log.finish(Some(401), false);
                    last_error = format!("provider '{}' token refresh failed: {e}", provider.id);
                    continue;
                }
            }
        }

        match result {
            Attempt::BuildFailed(e) => {
                mark(provider, effective_model, &|st| st.mark_misconfigured(Instant::now()));
                log.finish(None, false);
                last_error = e;
            }
            Attempt::SendFailed(e) => {
                mark(provider, effective_model, &backoff_retry);
                log.finish(None, false);
                tracing::warn!(provider = %provider.id, error = %e, "image upstream request error");
                last_error = format!("upstream request error: {e}");
            }
            Attempt::TimedOut => {
                mark(provider, effective_model, &backoff_retry);
                log.finish(None, false);
                // Upstream may still be generating (and spending quota):
                // don't start the same prompt on another account.
                return image_error(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", "image generation timed out");
            }
            Attempt::ReadFailed(e) => {
                mark(provider, effective_model, &backoff_retry);
                log.finish(Some(200), false);
                tracing::warn!(provider = %provider.id, error = %e, "image upstream body read failed");
                return image_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_stream_incomplete",
                    &format!("image generation stream failed: {e}"),
                );
            }
            Attempt::Status { status, retry_after, body } => {
                let message = error_message(&body);
                tracing::warn!(provider = %provider.id, status, error = %message, "image upstream error status");
                log.finish(Some(status), false);
                match classify_status(status, retry_after, &body) {
                    // A second 401 right after a successful refresh.
                    PreStream::AuthExpired | PreStream::Misconfigured => {
                        mark(provider, effective_model, &|st| st.mark_misconfigured(Instant::now()))
                    }
                    PreStream::Retryable(cooldown) => mark(provider, effective_model, &|st| match cooldown {
                        Some(c) => st.record_retryable(c, Instant::now()),
                        None => backoff_retry(st),
                    }),
                    PreStream::Relay => {
                        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                        return image_error(status, "upstream_rejected", &message);
                    }
                }
                last_error = format!("upstream status {status}: {message}");
            }
            Attempt::Sse(text) => match parse_stream(&text) {
                StreamOutcome::Image(img) => {
                    mark(provider, effective_model, &|st| st.record_success());
                    log.finish(Some(200), true);
                    return (StatusCode::OK, Json(success_body(*img))).into_response();
                }
                StreamOutcome::Failed { kind: FailureKind::UsageLimit(cooldown), message, .. } => {
                    mark(provider, effective_model, &|st| match cooldown {
                        Some(c) => st.record_retryable(c, Instant::now()),
                        None => backoff_retry(st),
                    });
                    log.finish(Some(429), false);
                    last_error = format!("usage limit: {message}");
                }
                StreamOutcome::Failed { kind: FailureKind::Rejected, code, message } => {
                    log.finish(Some(400), false);
                    return image_error(StatusCode::BAD_REQUEST, &code, &message);
                }
                StreamOutcome::Failed { kind: FailureKind::Unknown, code, message } => {
                    log.finish(Some(502), false);
                    return image_error(StatusCode::BAD_GATEWAY, &code, &message);
                }
                StreamOutcome::Refused(text) => {
                    log.finish(Some(400), false);
                    return image_error(StatusCode::BAD_REQUEST, "image_generation_refused", &text);
                }
                StreamOutcome::NoImage => {
                    // Completed without an image or a word: this account
                    // can't generate images (e.g. a Free plan).
                    mark(provider, effective_model, &|st| st.mark_misconfigured(Instant::now()));
                    log.finish(Some(200), false);
                    last_error = format!("provider '{}' returned no image", provider.id);
                }
                StreamOutcome::Incomplete => {
                    log.finish(Some(200), false);
                    return image_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_stream_incomplete",
                        "image generation stream ended early",
                    );
                }
            },
        }
    }
    image_error(StatusCode::SERVICE_UNAVAILABLE, "no_provider_available", &last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Result<(String, ImageParams), String> {
        parse_request(&v)
    }

    #[test]
    fn accepts_a_minimal_and_a_full_request() {
        let (model, p) = parse(json!({"model": "img", "prompt": " a fox "})).unwrap();
        assert_eq!(model, "img");
        assert_eq!(p.prompt, "a fox");
        assert_eq!(p.output_format, "png");
        let (_, p) = parse(json!({"model": "img", "prompt": "x", "n": 1, "size": "1536x1024",
            "quality": "high", "background": "transparent", "output_format": "webp",
            "response_format": "b64_json", "stream": false, "user": "u"}))
        .unwrap();
        assert_eq!(p.size.as_deref(), Some("1536x1024"));
        assert_eq!(p.quality.as_deref(), Some("high"));
        assert_eq!(p.background.as_deref(), Some("transparent"));
        assert_eq!(p.output_format, "webp");
    }

    #[test]
    fn rejects_unsupported_requests() {
        for bad in [
            json!({"prompt": "x"}),
            json!({"model": "img"}),
            json!({"model": "img", "prompt": "   "}),
            json!({"model": "img", "prompt": "x", "n": 2}),
            json!({"model": "img", "prompt": "x", "response_format": "url"}),
            json!({"model": "img", "prompt": "x", "stream": true}),
            json!({"model": "img", "prompt": "x", "size": "7x7"}),
            json!({"model": "img", "prompt": "x", "size": "big"}),
            json!({"model": "img", "prompt": "x", "quality": "ultra"}),
            json!({"model": "img", "prompt": "x", "background": "green"}),
            json!({"model": "img", "prompt": "x", "output_format": "gif"}),
            json!({"model": "img", "prompt": "x".repeat(MAX_PROMPT_BYTES + 1)}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn success_body_has_optional_fields_only_when_known() {
        let body = success_body(crate::media::codex_images::GeneratedImage {
            b64_json: "QUJD".into(),
            size: Some("1024x1024".into()),
            ..Default::default()
        });
        assert_eq!(body["data"][0]["b64_json"], "QUJD");
        assert!(body["data"][0].get("revised_prompt").is_none());
        assert_eq!(body["size"], "1024x1024");
        assert!(body.get("usage").is_none());
    }
}
