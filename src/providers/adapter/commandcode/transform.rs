use axum::http::StatusCode;
use bytes::Bytes;
use chrono::Utc;
use futures::Stream;
use serde_json::{json, Map, Value};
use uuid::Uuid;

const DEFAULT_MAX_TOKENS: i64 = 64_000;

fn environment_info() -> String {
    format!(
        "{}-{}, 1router/{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION")
    )
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn complete_tool_call_ids(messages: &[Value]) -> std::collections::HashSet<String> {
    let calls: std::collections::HashSet<String> = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| call["id"].as_str().map(str::to_string))
        .collect();
    let results: std::collections::HashSet<String> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["tool_call_id"].as_str().map(str::to_string))
        .collect();
    calls.intersection(&results).cloned().collect()
}

fn convert_messages(messages: &[Value]) -> Vec<Value> {
    let paired = complete_tool_call_ids(messages);
    let mut out = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        match role {
            "system" => {}
            "assistant" => {
                let mut content = Vec::new();
                if let Some(text) = message.get("content").filter(|v| !v.is_null()) {
                    let text = content_text(text);
                    if !text.is_empty() {
                        content.push(json!({"type":"text", "text":text}));
                    }
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"].as_str().unwrap_or_default();
                    if !paired.contains(id) {
                        continue;
                    }
                    let input = call["function"]["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    content.push(json!({
                        "type":"tool-call",
                        "toolCallId":id,
                        "toolName":call["function"]["name"].as_str().unwrap_or_default(),
                        "input":input
                    }));
                }
                if !content.is_empty() {
                    out.push(json!({"role":"assistant", "content":content}));
                }
            }
            "tool" => {
                let id = message["tool_call_id"].as_str().unwrap_or_default();
                if paired.contains(id) {
                    out.push(json!({
                        "role":"tool",
                        "content":[{"type":"tool-result","toolCallId":id,"output":{"type":"text","value":content_text(&message["content"])}}]
                    }));
                }
            }
            _ => {
                // Command Code's upstream speaks Anthropic-style content
                // blocks - it accepts `{type: "image", source: {...}}` but
                // rejects OpenAI's `{type: "image_url", image_url: {...}}`.
                // OpenAI clients (or the claude_bridge's Anthropic->OpenAI
                // leg, which converts image/source to image_url) therefore
                // need their image parts re-encoded before forwarding.
                let content = message.get("content").cloned().unwrap_or(Value::Null);
                out.push(json!({
                    "role": role,
                    "content": convert_user_content(content),
                }));
            }
        }
    }
    out
}

/// Rewrite OpenAI `image_url` content parts into Command Code's expected
/// Anthropic-style `image` + `source` blocks; everything else passes through
/// unchanged. A string content is returned as-is (both sides accept it).
fn convert_user_content(content: Value) -> Value {
    match content {
        Value::String(_) => content,
        Value::Array(parts) => Value::Array(
            parts
                .into_iter()
                .map(|part| {
                    if part.get("type").and_then(Value::as_str) == Some("image_url") {
                        let url = part["image_url"]["url"].as_str().unwrap_or("");
                        if let Some(rest) = url.strip_prefix("data:") {
                            let (media_type, data) = rest
                                .split_once(";base64,")
                                .map(|(m, d)| (m.to_string(), d.to_string()))
                                .unwrap_or_else(|| ("image/png".into(), url.into()));
                            return json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": media_type, "data": data }
                            });
                        }
                        // Remote (http) image URLs: pass through as an
                        // `image` block with a url source.
                        return json!({
                            "type": "image",
                            "source": { "type": "url", "url": url }
                        });
                    }
                    part
                })
                .collect(),
        ),
        _ => content,
    }
}

fn convert_tools(tools: Option<&Vec<Value>>) -> Vec<Value> {
    tools
        .into_iter()
        .flatten()
        .map(|tool| {
            let function = &tool["function"];
            json!({
                "type":"function",
                "name":function["name"].as_str().unwrap_or_default(),
                "description":function["description"].as_str().unwrap_or_default(),
                "input_schema":function.get("parameters").cloned().unwrap_or_else(|| json!({}))
            })
        })
        .collect()
}

fn system_prompt(messages: &[Value]) -> Option<String> {
    let values: Vec<String> = messages
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| content_text(&message["content"]))
        .filter(|text| !text.is_empty())
        .collect();
    (!values.is_empty()).then(|| values.join("\n\n"))
}

pub fn transform_request(client_json: &Value, thread_id: &str, working_dir: &str) -> Value {
    let model = client_json.get("model").cloned().unwrap_or(Value::Null);
    let messages = client_json["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let max_tokens = client_json["max_tokens"]
        .as_i64()
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .clamp(1, DEFAULT_MAX_TOKENS);
    let mut params = json!({
        "model": model,
        "messages": convert_messages(&messages),
        "tools": convert_tools(client_json["tools"].as_array()),
        "max_tokens": max_tokens,
        "temperature": 0.3,
        "stream": true
    });
    // commandcode.ai's API validates `params.system` as a string, not
    // nullable - a request with no system message (e.g. the admin UI's
    // validate-model probe, or any real chat with no system prompt) sent
    // `null` here and was rejected with a 400 ("expected string, received
    // null at \"params.system\"") before this was caught.
    params["system"] = Value::String(system_prompt(&messages).unwrap_or_default());

    json!({
        "config": {
            "workingDir": working_dir,
            "date": Utc::now().format("%Y-%m-%d").to_string(),
            "environment": environment_info(),
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": []
        },
        "memory": Value::Null,
        "taste": Value::Null,
        "skills": Value::Null,
        "params": params,
        "threadId": thread_id
    })
}

pub fn parse_event_line(line: &str) -> Option<Value> {
    let mut line = line.trim();
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return None;
    }
    if let Some(rest) = line.strip_prefix("data:") {
        line = rest.trim();
    }
    if line.is_empty() || line == "[DONE]" {
        return None;
    }
    serde_json::from_str(line).ok()
}

#[derive(Default)]
pub struct ChunkState {
    id: String,
    created: i64,
    saw_tool_call: bool,
}

impl ChunkState {
    fn ensure_metadata(&mut self) {
        if self.id.is_empty() {
            self.id = format!("chatcmpl-{}", Uuid::new_v4());
            self.created = Utc::now().timestamp();
        }
    }
}

fn chunk(state: &mut ChunkState, model: &str, delta: Value, finish_reason: Option<&str>) -> Value {
    state.ensure_metadata();
    json!({
        "id":state.id,
        "object":"chat.completion.chunk",
        "created":state.created,
        "model":model,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]
    })
}

fn finish_reason(value: Option<&str>) -> &'static str {
    match value {
        Some("length" | "max_tokens" | "max-tokens" | "max_output_tokens") => "length",
        Some("tool-calls") => "tool_calls",
        _ => "stop",
    }
}

fn usage(value: &Value) -> Value {
    let input = value["inputTokens"].as_i64().unwrap_or(0);
    let output = value["outputTokens"].as_i64().unwrap_or(0);
    let cached = value["inputTokenDetails"]["cacheReadTokens"]
        .as_i64()
        .unwrap_or(0);
    let mut out =
        json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output});
    if cached > 0 {
        out["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    out
}

pub fn chat_chunk_for_event(state: &mut ChunkState, event: &Value, model: &str) -> Option<Value> {
    match event["type"].as_str()? {
        "text-delta" => Some(chunk(
            state,
            model,
            json!({"content":event["text"].as_str().unwrap_or_default()}),
            None,
        )),
        "reasoning-delta" => Some(chunk(
            state,
            model,
            json!({"reasoning_content":event["text"].as_str().unwrap_or_default()}),
            None,
        )),
        "tool-call" => {
            state.saw_tool_call = true;
            let input = serde_json::to_string(&event["input"]).unwrap_or_else(|_| "{}".into());
            Some(chunk(
                state,
                model,
                json!({"tool_calls":[{"index":0,"id":event["toolCallId"],"type":"function","function":{"name":event["toolName"],"arguments":input}}]}),
                None,
            ))
        }
        "finish" => {
            let reason = match event["finishReason"].as_str() {
                Some(value) => finish_reason(Some(value)),
                None if state.saw_tool_call => "tool_calls",
                None => "stop",
            };
            let mut out = chunk(state, model, json!({}), Some(reason));
            if event.get("totalUsage").is_some() {
                out["usage"] = usage(&event["totalUsage"]);
            }
            Some(out)
        }
        _ => None,
    }
}

pub fn render_chunk(chunk: &Value) -> Bytes {
    Bytes::from(format!("data: {chunk}\n\n"))
}

/// Map a commandcode `{"type":"error",...}` NDJSON event to an OpenAI
/// chat.completion.chunk that carries the error as content + a
/// `finish_reason` so streaming clients (OpenAI SSE, or Anthropic via the
/// claude_bridge) see the failure instead of a silent `[DONE]`.
pub fn error_chunk_for_event(state: &mut ChunkState, event: &Value, model: &str) -> Option<Value> {
    let message = event["error"]
        .as_str()
        .map(str::to_string)
        .or_else(|| event["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| "upstream error".into());
    let mut out = chunk(
        state,
        model,
        json!({"content": format!("[commandcode error: {message}]")}),
        Some("stop"),
    );
    out["error"] = json!({"message": message, "type": "upstream_error"});
    Some(out)
}

pub const SSE_DONE: &[u8] = b"data: [DONE]\n\n";

pub fn convert_ndjson_stream<S, E>(
    upstream: S,
    model: String,
) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    struct State<S> {
        upstream: S,
        buffer: String,
        chunks: ChunkState,
        model: String,
        done: bool,
    }
    futures::stream::unfold(
        State {
            upstream,
            buffer: String::new(),
            chunks: ChunkState::default(),
            model,
            done: false,
        },
        |mut state| async move {
            use futures::StreamExt;
            loop {
                if state.done {
                    return None;
                }
                if let Some(pos) = state.buffer.find('\n') {
                    let line: String = state.buffer.drain(..=pos).collect();
                    if let Some(event) = parse_event_line(&line) {
                        // An error event mid-stream (e.g. a vision request the
                        // model rejects) must surface to the client, not be
                        // silently swallowed into a bare `[DONE]`.
                        if event["type"] == "error" {
                            state.done = true;
                            return Some((
                                Ok(error_chunk_for_event(
                                    &mut state.chunks,
                                    &event,
                                    &state.model,
                                )
                                .map(|c| render_chunk(&c))
                                .unwrap_or_else(|| Bytes::from_static(SSE_DONE))),
                                state,
                            ));
                        }
                        if let Some(chunk) =
                            chat_chunk_for_event(&mut state.chunks, &event, &state.model)
                        {
                            return Some((Ok(render_chunk(&chunk)), state));
                        }
                    }
                    continue;
                }
                match state.upstream.next().await {
                    Some(Ok(bytes)) => state.buffer.push_str(&String::from_utf8_lossy(&bytes)),
                    Some(Err(error)) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        if !state.buffer.is_empty() {
                            if let Some(event) = parse_event_line(&state.buffer) {
                                state.buffer.clear();
                                // Trailing error event (no trailing newline or
                                // split across chunks) must surface too.
                                if event["type"] == "error" {
                                    state.done = true;
                                    return Some((
                                        Ok(error_chunk_for_event(
                                            &mut state.chunks,
                                            &event,
                                            &state.model,
                                        )
                                        .map(|c| render_chunk(&c))
                                        .unwrap_or_else(|| Bytes::from_static(SSE_DONE))),
                                        state,
                                    ));
                                }
                                if let Some(chunk) =
                                    chat_chunk_for_event(&mut state.chunks, &event, &state.model)
                                {
                                    return Some((Ok(render_chunk(&chunk)), state));
                                }
                            }
                        }
                        state.done = true;
                        return Some((Ok(Bytes::from_static(SSE_DONE)), state));
                    }
                }
            }
        },
    )
}

fn events(body: &str) -> impl Iterator<Item = Value> + '_ {
    body.lines().filter_map(parse_event_line)
}

pub fn aggregate_ndjson(body: &str, model: &str) -> Value {
    let mut state = ChunkState::default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = "stop";
    let mut usage_value = Value::Null;
    let mut tool_calls = Vec::new();
    for event in events(body) {
        match event["type"].as_str() {
            Some("text-delta") => content.push_str(event["text"].as_str().unwrap_or_default()),
            Some("reasoning-delta") => {
                reasoning.push_str(event["text"].as_str().unwrap_or_default())
            }
            Some("tool-call") => {
                state.saw_tool_call = true;
                tool_calls.push(json!({"id":event["toolCallId"],"type":"function","function":{"name":event["toolName"],"arguments":serde_json::to_string(&event["input"]).unwrap_or_else(|_| "{}".into())}}));
            }
            Some("finish") => {
                finish = finish_reason(event["finishReason"].as_str());
                if event.get("totalUsage").is_some() {
                    usage_value = usage(&event["totalUsage"]);
                }
            }
            _ => {}
        }
    }
    state.ensure_metadata();
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(content));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let mut out = json!({"id":state.id,"object":"chat.completion","model":model,"choices":[{"index":0,"message":Value::Object(message),"finish_reason":finish} ]});
    if !usage_value.is_null() {
        out["usage"] = usage_value;
    }
    out
}

pub fn ndjson_embedded_error(body: &str) -> Option<String> {
    events(body).find_map(|event| {
        if event["type"] != "error" {
            return None;
        }
        event["error"]
            .as_str()
            .map(str::to_string)
            .or_else(|| event["error"]["message"].as_str().map(str::to_string))
            .or_else(|| Some("upstream error".into()))
    })
}

/// Extract the `<NNN>` status prefix commandcode's embedded error events
/// carry (e.g. `<400> InternalError.Algo.InvalidParameter: ...`) so the
/// proxy can relay the real HTTP status instead of a generic 502/503.
pub fn embedded_error_status(message: &str) -> Option<StatusCode> {
    let rest = message.strip_prefix('<')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    StatusCode::from_u16(digits.parse().ok()?).ok()
}

pub fn project_slug_from_path(path: &str) -> String {
    let path = path
        .strip_prefix(|c: char| c.is_ascii_alphabetic())
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(path);
    let mut slug = String::new();
    let mut separator = false;
    for ch in path.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(ch);
        } else {
            separator = true;
        }
    }
    slug.trim_matches('-').to_string().if_empty("project")
}

trait StringIfEmpty {
    fn if_empty(self, fallback: impl Into<String>) -> String;
}

impl StringIfEmpty for String {
    fn if_empty(self, fallback: impl Into<String>) -> String {
        if self.is_empty() {
            fallback.into()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    #[test]
    fn transform_request_builds_the_command_code_envelope() {
        let input = json!({"model":"pool-x","messages":[{"role":"user","content":"hi"}],"max_tokens":100,"stream":true});
        let out = transform_request(&input, "thread-1", "/tmp/project");
        for key in ["config", "memory", "taste", "skills", "params", "threadId"] {
            assert!(out.get(key).is_some(), "missing {key}");
        }
        assert_eq!(out["params"]["stream"], true);
        assert_eq!(
            out["params"]["messages"][0],
            json!({"role":"user","content":"hi"})
        );
        assert_eq!(out["params"]["temperature"], 0.3);
        assert_eq!(out["params"]["max_tokens"], 100);
        assert!(out["memory"].is_null());
        assert!(out["taste"].is_null());
        assert!(out["skills"].is_null());
        assert_eq!(out["threadId"], "thread-1");
    }

    #[test]
    fn transform_request_lifts_system_messages_into_params_system() {
        let input = json!({"model":"m","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"}]});
        let out = transform_request(&input, "t", "/p");
        assert_eq!(out["params"]["system"], "be terse");
        assert_eq!(
            out["params"]["messages"],
            json!([{"role":"user","content":"hi"}])
        );
    }

    #[test]
    fn transform_request_sends_an_empty_string_system_when_no_system_message_is_present() {
        // Regression: commandcode.ai validates `params.system` as a string,
        // not nullable - sending `null` here (the prior behavior for a
        // system-message-less request) was rejected with a 400.
        let input = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let out = transform_request(&input, "t", "/p");
        assert_eq!(out["params"]["system"], "");
    }

    #[test]
    fn transform_request_converts_tools_and_tool_calls() {
        let input = json!({"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"t1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},{"role":"tool","tool_call_id":"t1","content":"ok"},{"role":"assistant","tool_calls":[{"id":"orphan","type":"function","function":{"name":"nope","arguments":"{}"}}]},{"role":"tool","tool_call_id":"missing","content":"drop"}],"tools":[{"type":"function","function":{"name":"lookup","description":"Find","parameters":{"type":"object"}}}]});
        let out = transform_request(&input, "t", "/p");
        assert_eq!(
            out["params"]["tools"],
            json!([{"type":"function","name":"lookup","description":"Find","input_schema":{"type":"object"}}])
        );
        assert_eq!(
            out["params"]["messages"][0]["content"][0],
            json!({"type":"tool-call","toolCallId":"t1","toolName":"lookup","input":{"q":"x"}})
        );
        assert_eq!(
            out["params"]["messages"][1]["content"][0],
            json!({"type":"tool-result","toolCallId":"t1","output":{"type":"text","value":"ok"}})
        );
        assert_eq!(out["params"]["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn convert_user_content_rewrites_image_url_to_image_source() {
        let input = json!({
            "role":"user",
            "content":[
                {"type":"text","text":"look at this"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},
                {"type":"image_url","image_url":{"url":"https://example.com/pic.png"}}
            ]
        });
        let out = convert_user_content(input["content"].clone());
        assert_eq!(
            out[0],
            json!({"type":"text","text":"look at this"})
        );
        assert_eq!(
            out[1],
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}})
        );
        assert_eq!(
            out[2],
            json!({"type":"image","source":{"type":"url","url":"https://example.com/pic.png"}})
        );
        assert_eq!(convert_user_content(json!("plain string")), json!("plain string"));
    }

    #[test]
    fn transform_request_keeps_string_content_as_is() {
        let input = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let out = transform_request(&input, "t", "/p");
        assert_eq!(out["params"]["messages"][0]["content"], "hi");
    }

    #[test]
    fn error_chunk_for_event_surfaces_the_message() {
        let mut state = ChunkState::default();
        let chunk = error_chunk_for_event(
            &mut state,
            &json!({"type":"error","error":{"message":"<400> bad image"}}),
            "m",
        )
        .unwrap();
        assert_eq!(
            chunk["choices"][0]["delta"]["content"],
            "[commandcode error: <400> bad image]"
        );
        assert_eq!(chunk["error"]["message"], "<400> bad image");
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn convert_ndjson_stream_surfaces_an_error_event_instead_of_silent_done() {
        let body = concat!(
            r#"{"type":"text-delta","text":"hi"}"#,
            "\n",
            r#"{"type":"error","error":{"message":"<400> bad image"}}"#,
            "\n"
        );
        let chunks = vec![Ok::<Bytes, ()>(Bytes::from(body))];
        let output = convert_ndjson_stream(futures::stream::iter(chunks), "m".into())
            .try_collect::<Vec<Bytes>>()
            .await
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("hi"), "text delta still emitted");
        assert!(
            text.contains("[commandcode error: <400> bad image]"),
            "error surfaced, got: {text}"
        );
        assert!(!text.contains("[DONE]"), "no silent [DONE] after an error");
    }

    #[tokio::test]
    async fn convert_ndjson_stream_surfaces_a_trailing_error_without_newline() {
        // The real upstream (generate transport) emits `{"type":"start"}\n`
        // then `{"type":"error",...}` with NO trailing newline, and the two
        // events can arrive split across network chunks.
        let chunks = vec![
            Ok::<Bytes, ()>(Bytes::from(r#"{"type":"start"}"#)),
            Ok(Bytes::from(
                r#"
{"type":"error","error":{"message":"<400> bad image","statusCode":400}}"#,
            )),
        ];
        let output = convert_ndjson_stream(futures::stream::iter(chunks), "m".into())
            .try_collect::<Vec<Bytes>>()
            .await
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("[commandcode error: <400> bad image]"),
            "trailing error surfaced, got: {text}"
        );
        assert!(!text.contains("[DONE]"), "no silent [DONE] after an error");
    }

    #[test]
    fn parse_event_line_is_tolerant() {
        let event = json!({"type":"text-delta","text":"a"});
        assert_eq!(
            parse_event_line(r#"{"type":"text-delta","text":"a"}"#),
            Some(event.clone())
        );
        assert_eq!(
            parse_event_line(r#"data: {"type":"text-delta","text":"a"}"#),
            Some(event)
        );
        for line in ["", ":comment", "event: foo", "data: [DONE]"] {
            assert!(parse_event_line(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn chat_chunk_for_event_maps_text_and_reasoning() {
        let mut state = ChunkState::default();
        let text = chat_chunk_for_event(&mut state, &json!({"type":"text-delta","text":"a"}), "m")
            .unwrap();
        assert_eq!(text["choices"][0]["delta"]["content"], "a");
        let reasoning = chat_chunk_for_event(
            &mut state,
            &json!({"type":"reasoning-delta","text":"think"}),
            "m",
        )
        .unwrap();
        assert_eq!(
            reasoning["choices"][0]["delta"]["reasoning_content"],
            "think"
        );
        assert!(
            chat_chunk_for_event(&mut state, &json!({"type":"reasoning-start"}), "m").is_none()
        );
        assert!(chat_chunk_for_event(&mut state, &json!({"type":"reasoning-end"}), "m").is_none());
    }

    #[test]
    fn chat_chunk_for_event_maps_tool_calls() {
        let mut state = ChunkState::default();
        let chunk = chat_chunk_for_event(
            &mut state,
            &json!({"type":"tool-call","toolCallId":"t1","toolName":"lookup","input":{"q":"x"}}),
            "m",
        )
        .unwrap();
        let call = &chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "t1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "lookup");
        assert_eq!(call["function"]["arguments"], r#"{"q":"x"}"#);
    }

    #[test]
    fn finish_event_maps_usage_and_finish_reason() {
        let event = json!({"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":100,"outputTokens":20,"inputTokenDetails":{"noCacheTokens":40,"cacheReadTokens":50,"cacheWriteTokens":10}}});
        let mut state = ChunkState::default();
        let chunk = chat_chunk_for_event(&mut state, &event, "m").unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            chunk["usage"],
            json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":50}})
        );
        for reason in ["length", "max_tokens", "max-tokens", "max_output_tokens"] {
            let chunk = chat_chunk_for_event(
                &mut state,
                &json!({"type":"finish","finishReason":reason}),
                "m",
            )
            .unwrap();
            assert_eq!(chunk["choices"][0]["finish_reason"], "length");
        }
        let chunk = chat_chunk_for_event(
            &mut state,
            &json!({"type":"finish","finishReason":"other"}),
            "m",
        )
        .unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn convert_ndjson_stream_emits_framed_sse_terminated_by_done() {
        let body = concat!(
            r#"{"type":"text-delta","text":"a"}"#,
            "\n",
            r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":1,"outputTokens":1}}"#,
            "\n"
        );
        let chunks = vec![
            Ok::<Bytes, ()>(Bytes::from(&body.as_bytes()[..7])),
            Ok(Bytes::from(&body.as_bytes()[7..19])),
            Ok(Bytes::from(&body.as_bytes()[19..])),
        ];
        let output = convert_ndjson_stream(futures::stream::iter(chunks), "m".into())
            .try_collect::<Vec<Bytes>>()
            .await
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        let text = String::from_utf8(output).unwrap();
        let items: Vec<&str> = text.split_inclusive("\n\n").collect();
        assert!(items.len() >= 3);
        for item in &items {
            assert!(item.starts_with("data: "));
            assert!(item.ends_with("\n\n"));
            if *item != "data: [DONE]\n\n" {
                let payload = item.strip_prefix("data: ").unwrap().trim();
                let json: Value = serde_json::from_str(payload).unwrap();
                assert_eq!(json["object"], "chat.completion.chunk");
            }
        }
        assert_eq!(items.last().copied(), Some("data: [DONE]\n\n"));
    }

    #[test]
    fn aggregate_ndjson_builds_a_chat_completion() {
        let body = concat!(
            r#"{"type":"text-delta","text":"hello "}"#,
            "\n",
            r#"{"type":"text-delta","text":"world"}"#,
            "\n",
            r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":2,"outputTokens":3}}"#,
            "\n"
        );
        let out = aggregate_ndjson(body, "m");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hello world");
        assert_eq!(out["usage"]["total_tokens"], 5);
    }

    #[test]
    fn ndjson_embedded_error_detects_the_error_event() {
        assert_eq!(
            ndjson_embedded_error(r#"{"type":"error","error":{"message":"boom"}}"#),
            Some("boom".into())
        );
        assert_eq!(
            ndjson_embedded_error(r#"{"type":"error","error":"boom"}"#),
            Some("boom".into())
        );
    }

    #[test]
    fn embedded_error_status_parses_the_prefix() {
        assert_eq!(
            embedded_error_status("<400> InternalError.Algo.InvalidParameter: bad image"),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            embedded_error_status("<429> rate limited"),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(embedded_error_status("no prefix here"), None);
        assert_eq!(embedded_error_status(""), None);
        assert_eq!(embedded_error_status("<99999> out of range"), None);
    }

    #[test]
    fn project_slug_from_path_matches_reference_rules() {
        assert_eq!(
            project_slug_from_path(r#"C:\Work\My Project"#),
            "work-my-project"
        );
        assert_eq!(project_slug_from_path("/tmp/My_Project"), "tmp-my-project");
        assert_eq!(project_slug_from_path("!!!"), "project");
    }
}
