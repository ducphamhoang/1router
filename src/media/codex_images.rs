//! Image generation through the Codex Responses endpoint: the request body
//! (an `image_generation` tool call), status classification, and parsing of
//! the buffered SSE body. Pure functions; `media::images` drives them.
//!
//! Upstream facts (P0 spike, real Plus account - see the plan's "P0
//! findings"): tool parameters are hints only (upstream normalizes them),
//! the final image arrives in `response.output_item.done`, an assistant
//! `message` item always accompanies it (usually empty), image usage is in
//! `response.completed.response.tool_usage.image_gen`, and `keepalive`
//! events fill long gaps.

use std::time::Duration;

use serde_json::{json, Value};

use crate::providers::adapter::codex::transform::sse_events;
use crate::providers::adapter::Credentials;

/// Upper bound for a usage-limit cooldown taken from an upstream body.
pub const MAX_USAGE_LIMIT_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);

/// Tool model sent for every `gpt-image-*` client model: upstream ignores the
/// value (it always runs its own Codex image model), so client names are
/// accepted for SDK compatibility without pretending they select anything.
const TOOL_MODEL: &str = "gpt-image-2";

const UNSUPPORTED_HOST_MODEL: &str = "model is not supported when using Codex";

/// Validated client parameters (`media::images` does the validation).
#[derive(Clone, Debug, Default)]
pub struct ImageParams {
    pub prompt: String,
    pub size: Option<String>,
    pub quality: Option<String>,
    pub background: Option<String>,
    pub output_format: String,
}

/// The Responses request body for `model` (a pool member's effective model:
/// `gpt-image-*` or `<chat-model>-image`).
pub fn build_body(model: &str, host_model: &str, p: &ImageParams) -> Value {
    let mut tool = json!({ "type": "image_generation", "output_format": p.output_format });
    for (key, value) in [("size", &p.size), ("quality", &p.quality), ("background", &p.background)] {
        if let Some(v) = value {
            tool[key] = json!(v);
        }
    }
    let alias_host = model.strip_suffix("-image").filter(|_| !model.starts_with("gpt-image-"));
    let (host, tool_choice) = match alias_host {
        Some(host) => (host.to_string(), json!("auto")),
        None => {
            tool["model"] = json!(TOOL_MODEL);
            tool["action"] = json!("generate");
            (host_model.to_string(), json!({ "type": "image_generation" }))
        }
    };
    let mut body = json!({
        "model": host,
        "instructions": "",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": prompt_with_hints(p) }]
        }],
        "tools": [tool],
        "tool_choice": tool_choice,
        "parallel_tool_calls": false,
        // Fresh per request: never the chat adapter's per-pool cache key.
        "prompt_cache_key": uuid::Uuid::new_v4().to_string(),
        "stream": true,
        "store": false,
    });
    if alias_host.is_none() {
        body["reasoning"] = json!({ "effort": "medium", "summary": "auto" });
    }
    body
}

/// Upstream ignores the tool's size/quality/background, so also state them in
/// the prompt - the host model picks the real values and may follow them.
fn prompt_with_hints(p: &ImageParams) -> String {
    let hints: Vec<String> = [("size", &p.size), ("quality", &p.quality), ("background", &p.background)]
        .into_iter()
        .filter_map(|(k, v)| v.as_deref().filter(|v| *v != "auto").map(|v| format!("{k}: {v}")))
        .collect();
    if hints.is_empty() {
        p.prompt.clone()
    } else {
        format!("{}\n\n(Requested output - {}.)", p.prompt, hints.join(", "))
    }
}

/// Same headers as the chat adapter (verified sufficient in P0).
pub fn build_request(
    http: &reqwest::Client,
    url: &str,
    creds: &Credentials,
    body: &Value,
) -> Result<reqwest::Request, String> {
    let access = creds
        .access_token
        .as_deref()
        .ok_or_else(|| "codex provider missing access_token".to_string())?;
    let mut builder = http
        .post(url)
        .json(body)
        .bearer_auth(access)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", format!("codex_cli_rs/{}", env!("CARGO_PKG_VERSION")))
        .header("accept", "text/event-stream")
        .header("session_id", uuid::Uuid::new_v4().to_string());
    if let Some(account_id) = creds.provider_data["chatgpt_account_id"].as_str().filter(|s| !s.is_empty()) {
        builder = builder.header("ChatGPT-Account-ID", account_id);
    }
    builder.build().map_err(|e| format!("codex image request build failed: {}", e.without_url()))
}

/// What to do with a non-2xx status (which Codex sends before any
/// generation starts, so no quota was spent).
#[derive(Debug, PartialEq)]
pub enum PreStream {
    /// 401: refresh the token and retry the same member once.
    AuthExpired,
    /// Fail over; skip this member for the given cooldown (`None` = the
    /// caller's escalating backoff).
    Retryable(Option<Duration>),
    /// Fail over; the account can't serve this (403, unsupported host model).
    Misconfigured,
    /// The request itself is bad: relay to the client, no failover.
    Relay,
}

pub fn classify_status(status: u16, retry_after: Option<Duration>, body: &str) -> PreStream {
    match status {
        401 => PreStream::AuthExpired,
        403 => PreStream::Misconfigured,
        429 => PreStream::Retryable(
            retry_after.or_else(|| serde_json::from_str::<Value>(body).ok().and_then(|v| usage_limit_cooldown(&v))),
        ),
        400 if body.contains(UNSUPPORTED_HOST_MODEL) => PreStream::Misconfigured,
        400 | 413 | 422 => PreStream::Relay,
        408 | 500..=599 => PreStream::Retryable(None),
        // 404, 3xx (redirects are never followed, SEC-10), other 4xx.
        _ => PreStream::Retryable(Some(Duration::from_secs(30))),
    }
}

/// `resets_in_seconds` / `resets_at` (unix seconds) from a usage-limit error,
/// at the top level or under `error` (9router `executors/codex.js`), capped.
pub fn usage_limit_cooldown(v: &Value) -> Option<Duration> {
    let find = |key: &str| v.get(key).or_else(|| v.get("error").and_then(|e| e.get(key))).and_then(Value::as_f64);
    let secs = find("resets_in_seconds").or_else(|| {
        find("resets_at").map(|at| at - chrono::Utc::now().timestamp() as f64)
    })?;
    (secs > 0.0).then(|| Duration::from_secs_f64(secs).min(MAX_USAGE_LIMIT_COOLDOWN))
}

/// A readable message from an upstream error body (`{"detail": ..}`,
/// `{"error": {"message": ..}}`, or raw text), truncated.
pub fn error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let msg = parsed
        .as_ref()
        .and_then(|v| {
            v["detail"]
                .as_str()
                .or_else(|| v["error"]["message"].as_str())
                .or_else(|| v["message"].as_str())
                .or_else(|| v["error"].as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| body.to_string());
    msg.chars().take(500).collect()
}

#[derive(Debug, Default, PartialEq)]
pub struct GeneratedImage {
    pub b64_json: String,
    pub revised_prompt: Option<String>,
    /// Actual values upstream used (they can differ from what was asked).
    pub size: Option<String>,
    pub quality: Option<String>,
    pub background: Option<String>,
    pub output_format: Option<String>,
    /// Non-empty assistant text next to the image (e.g. an explanation that
    /// the prompt was rewritten).
    pub text: Option<String>,
    /// `tool_usage.image_gen` from `response.completed`.
    pub usage: Option<Value>,
}

#[derive(Debug, PartialEq)]
pub enum FailureKind {
    /// Usage / rate limit: fail over, cool down.
    UsageLimit(Option<Duration>),
    /// Moderation or invalid request: 400 to the client, no failover.
    Rejected,
    /// Anything else: 502, no failover.
    Unknown,
}

#[derive(Debug, PartialEq)]
pub enum StreamOutcome {
    Image(Box<GeneratedImage>),
    Failed { kind: FailureKind, code: String, message: String },
    /// `response.completed`, no image, non-empty assistant text.
    Refused(String),
    /// `response.completed`, no image, no text: the account can't generate
    /// images (e.g. a Free plan).
    NoImage,
    /// No `response.completed`: truncated stream.
    Incomplete,
}

fn failure_kind(code: &str, v: &Value) -> FailureKind {
    let c = code.to_ascii_lowercase();
    if c.contains("usage_limit") || c.contains("rate_limit") || c.contains("quota") {
        FailureKind::UsageLimit(usage_limit_cooldown(v))
    } else if c.contains("moderation")
        || c.contains("content_policy")
        || c.contains("safety")
        || c.contains("invalid")
        || c.contains("user_error")
    {
        FailureKind::Rejected
    } else {
        FailureKind::Unknown
    }
}

/// Classify a complete (buffered) SSE body.
pub fn parse_stream(sse_body: &str) -> StreamOutcome {
    let mut image: Option<GeneratedImage> = None;
    let mut text = String::new();
    let mut completed = false;
    let mut usage: Option<Value> = None;
    let mut failure: Option<StreamOutcome> = None;

    for (event, data) in sse_events(sse_body) {
        let kind = data["type"].as_str().map(str::to_string).unwrap_or(event);
        match kind.as_str() {
            "keepalive" => {}
            "response.output_item.done" => {
                let item = &data["item"];
                match item["type"].as_str() {
                    Some("image_generation_call") => {
                        if item["status"].as_str() == Some("failed") {
                            failure.get_or_insert(StreamOutcome::Failed {
                                kind: FailureKind::Rejected,
                                code: "image_generation_failed".into(),
                                message: "upstream image generation failed".into(),
                            });
                        } else if let Some(result) = item["result"].as_str().filter(|r| !r.is_empty()) {
                            let s = |k: &str| item[k].as_str().map(str::to_string);
                            image.get_or_insert(GeneratedImage {
                                b64_json: result.to_string(),
                                revised_prompt: s("revised_prompt"),
                                size: s("size"),
                                quality: s("quality"),
                                background: s("background"),
                                output_format: s("output_format"),
                                ..Default::default()
                            });
                        }
                    }
                    Some("message") => {
                        for part in item["content"].as_array().into_iter().flatten() {
                            if part["type"].as_str() == Some("output_text") {
                                text.push_str(part["text"].as_str().unwrap_or_default());
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" => {
                completed = true;
                usage = data["response"]["tool_usage"]["image_gen"].as_object().map(|_| {
                    data["response"]["tool_usage"]["image_gen"].clone()
                });
            }
            "response.failed" | "error" | "response.error" => {
                let err = if data["response"]["error"].is_object() {
                    &data["response"]["error"]
                } else if data["error"].is_object() {
                    &data["error"]
                } else {
                    &data
                };
                let code = err["code"]
                    .as_str()
                    .or_else(|| err["type"].as_str())
                    .unwrap_or("upstream_error")
                    .to_string();
                let message = err["message"].as_str().unwrap_or("upstream image generation error");
                failure.get_or_insert(StreamOutcome::Failed {
                    kind: failure_kind(&code, err),
                    code,
                    message: message.chars().take(500).collect(),
                });
            }
            _ => {}
        }
    }

    let text = text.trim().to_string();
    if let Some(mut image) = image {
        image.text = (!text.is_empty()).then_some(text);
        image.usage = usage;
        return StreamOutcome::Image(Box::new(image));
    }
    if let Some(failure) = failure {
        return failure;
    }
    if !completed {
        return StreamOutcome::Incomplete;
    }
    if text.is_empty() {
        StreamOutcome::NoImage
    } else {
        StreamOutcome::Refused(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ImageParams {
        ImageParams { prompt: "a fox".into(), output_format: "png".into(), ..Default::default() }
    }

    fn ev(v: Value) -> String {
        format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v)
    }

    fn image_done(result: &str) -> String {
        ev(json!({"type": "response.output_item.done", "item": {
            "type": "image_generation_call", "status": "completed", "result": result,
            "revised_prompt": "a red fox", "size": "1254x1254", "quality": "medium",
            "background": "opaque", "output_format": "png"}}))
    }

    fn message_done(text: &str) -> String {
        ev(json!({"type": "response.output_item.done", "item": {
            "type": "message", "content": [{"type": "output_text", "text": text}]}}))
    }

    fn completed() -> String {
        ev(json!({"type": "response.completed", "response": {"output": [],
            "tool_usage": {"image_gen": {"input_tokens": 10, "output_tokens": 20, "total_tokens": 30}}}}))
    }

    #[test]
    fn gpt_image_body_uses_the_host_model_and_forced_tool_choice() {
        let mut p = params();
        p.size = Some("1024x1024".into());
        p.quality = Some("low".into());
        let b = build_body("gpt-image-1.5", "gpt-5.5", &p);
        assert_eq!(b["model"], "gpt-5.5");
        assert_eq!(b["tool_choice"], json!({"type": "image_generation"}));
        assert_eq!(b["tools"][0]["model"], TOOL_MODEL);
        assert_eq!(b["tools"][0]["action"], "generate");
        assert_eq!(b["tools"][0]["size"], "1024x1024");
        assert_eq!(b["reasoning"]["effort"], "medium");
        assert_eq!(b["stream"], true);
        assert_eq!(b["store"], false);
        let text = b["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("a fox") && text.contains("size: 1024x1024") && text.contains("quality: low"));
        let other = build_body("gpt-image-1.5", "gpt-5.5", &p);
        assert_ne!(b["prompt_cache_key"], other["prompt_cache_key"], "fresh key per request");
    }

    #[test]
    fn image_alias_body_uses_its_chat_model_and_no_reasoning() {
        let b = build_body("gpt-5.5-image", "ignored", &params());
        assert_eq!(b["model"], "gpt-5.5");
        assert_eq!(b["tool_choice"], "auto");
        assert!(b.get("reasoning").is_none(), "reasoning must be omitted, not null");
        assert!(b["tools"][0].get("model").is_none());
        assert_eq!(b["input"][0]["content"][0]["text"], "a fox", "no hints when none given");
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify_status(401, None, ""), PreStream::AuthExpired);
        assert_eq!(classify_status(403, None, ""), PreStream::Misconfigured);
        assert_eq!(
            classify_status(400, None, r#"{"detail":"The 'x' model is not supported when using Codex with a ChatGPT account."}"#),
            PreStream::Misconfigured
        );
        assert_eq!(classify_status(400, None, r#"{"detail":"bad"}"#), PreStream::Relay);
        assert_eq!(classify_status(422, None, ""), PreStream::Relay);
        assert_eq!(classify_status(503, None, ""), PreStream::Retryable(None));
        assert_eq!(classify_status(404, None, ""), PreStream::Retryable(Some(Duration::from_secs(30))));
        assert_eq!(
            classify_status(429, None, r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":120}}"#),
            PreStream::Retryable(Some(Duration::from_secs(120)))
        );
        assert_eq!(
            classify_status(429, Some(Duration::from_secs(7)), r#"{"resets_in_seconds":120}"#),
            PreStream::Retryable(Some(Duration::from_secs(7))),
            "retry-after header wins"
        );
        assert_eq!(
            classify_status(429, None, r#"{"resets_in_seconds":999999}"#),
            PreStream::Retryable(Some(MAX_USAGE_LIMIT_COOLDOWN))
        );
    }

    #[test]
    fn error_message_reads_detail_error_or_raw() {
        assert_eq!(error_message(r#"{"detail":"nope"}"#), "nope");
        assert_eq!(error_message(r#"{"error":{"message":"bad size"}}"#), "bad size");
        assert_eq!(error_message("plain"), "plain");
    }

    #[test]
    fn image_with_empty_message_and_keepalives() {
        let body = [
            ev(json!({"type": "response.created"})),
            ev(json!({"type": "keepalive"})),
            image_done("QUJD"),
            message_done(""),
            completed(),
        ]
        .concat();
        let StreamOutcome::Image(img) = parse_stream(&body) else { panic!() };
        assert_eq!(img.b64_json, "QUJD");
        assert_eq!(img.revised_prompt.as_deref(), Some("a red fox"));
        assert_eq!(img.size.as_deref(), Some("1254x1254"));
        assert_eq!(img.text, None);
        assert_eq!(img.usage.as_ref().unwrap()["total_tokens"], 30);
    }

    #[test]
    fn text_next_to_an_image_is_carried_not_a_refusal() {
        let body = [image_done("QUJD"), message_done("Sorry, I made an original one instead."), completed()].concat();
        let StreamOutcome::Image(img) = parse_stream(&body) else { panic!() };
        assert_eq!(img.text.as_deref(), Some("Sorry, I made an original one instead."));
    }

    #[test]
    fn refusal_entitlement_and_truncation() {
        assert_eq!(
            parse_stream(&[message_done("I can't help with that."), completed()].concat()),
            StreamOutcome::Refused("I can't help with that.".into())
        );
        assert_eq!(parse_stream(&[message_done(""), completed()].concat()), StreamOutcome::NoImage);
        assert_eq!(parse_stream(&message_done("partial")), StreamOutcome::Incomplete);
        assert_eq!(parse_stream(""), StreamOutcome::Incomplete);
    }

    #[test]
    fn failure_shapes() {
        let failed = ev(json!({"type": "response.failed", "response": {"error": {
            "code": "usage_limit_reached", "message": "limit", "resets_in_seconds": 60}}}));
        assert_eq!(
            parse_stream(&failed),
            StreamOutcome::Failed {
                kind: FailureKind::UsageLimit(Some(Duration::from_secs(60))),
                code: "usage_limit_reached".into(),
                message: "limit".into()
            }
        );
        let err = ev(json!({"type": "error", "code": "moderation_blocked", "message": "blocked"}));
        assert!(matches!(parse_stream(&err), StreamOutcome::Failed { kind: FailureKind::Rejected, .. }));
        let weird = ev(json!({"type": "error", "code": "server_is_sad", "message": "?"}));
        assert!(matches!(parse_stream(&weird), StreamOutcome::Failed { kind: FailureKind::Unknown, .. }));
        let item_failed = ev(json!({"type": "response.output_item.done", "item": {
            "type": "image_generation_call", "status": "failed"}}));
        assert!(matches!(
            parse_stream(&[item_failed, completed()].concat()),
            StreamOutcome::Failed { kind: FailureKind::Rejected, .. }
        ));
    }

    #[test]
    fn an_image_event_larger_than_the_chat_sse_cap_parses() {
        let big = "A".repeat(17 * 1024 * 1024);
        let body = [image_done(&big), completed()].concat();
        let StreamOutcome::Image(img) = parse_stream(&body) else { panic!() };
        assert_eq!(img.b64_json.len(), big.len());
    }
}
