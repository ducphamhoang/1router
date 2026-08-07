// Bridges Anthropic Messages API shape <-> the OpenAI Chat-Completions shape
// that `transform.rs` already speaks to the Codex Responses API. Lets a
// Codex provider with `wire_format = "anthropic"` serve `/v1/messages`
// clients (Claude Code) directly, without touching the Responses-API
// transformation itself.
use bytes::Bytes;
use serde_json::{json, Value};

fn convert_tool_choice(choice: &Value) -> Value {
    match choice {
        Value::String(s) => json!(s),
        Value::Object(o) => match o.get("type").and_then(|t| t.as_str()) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("tool") => json!({
                "type": "function",
                "function": { "name": o.get("name").cloned().unwrap_or(json!("")) }
            }),
            _ => json!("auto"),
        },
        _ => json!("auto"),
    }
}

/// Anthropic Messages request body -> OpenAI Chat Completions request body.
pub fn claude_to_openai_request(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = body.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            messages.push(json!({ "role": "system", "content": system_text }));
        }
    }

    if let Some(Value::Array(msgs)) = body.get("messages") {
        for m in msgs {
            convert_claude_message(m, &mut messages);
        }
    }

    let mut out = json!({ "messages": messages });
    if let Some(model) = body.get("model") {
        out["model"] = model.clone();
    }
    if let Some(stream) = body.get("stream") {
        out["stream"] = stream.clone();
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        out["max_tokens"] = max_tokens.clone();
    }
    if let Some(temperature) = body.get("temperature") {
        out["temperature"] = temperature.clone();
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        let openai_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(json!("")),
                        "description": t.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned()
                            .unwrap_or(json!({"type": "object", "properties": {}})),
                    }
                })
            })
            .collect();
        out["tools"] = json!(openai_tools);
    }
    if let Some(tool_choice) = body.get("tool_choice") {
        out["tool_choice"] = convert_tool_choice(tool_choice);
    }

    out
}

fn convert_claude_message(msg: &Value, out: &mut Vec<Value>) {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = msg.get("content").cloned().unwrap_or(Value::Null);

    if let Value::String(s) = &content {
        out.push(json!({ "role": role, "content": s }));
        return;
    }
    let Value::Array(blocks) = content else {
        out.push(json!({ "role": role, "content": "" }));
        return;
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut image_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for block in &blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("image") => {
                if let Some(source) = block.get("source") {
                    if source.get("type").and_then(|t| t.as_str()) == Some("base64") {
                        let media_type = source
                            .get("media_type")
                            .and_then(|m| m.as_str())
                            .unwrap_or("image/png");
                        let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                        image_parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") }
                        }));
                    }
                }
            }
            Some("tool_use") => {
                tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or(json!("")),
                        "arguments": serde_json::to_string(block.get("input").unwrap_or(&json!({})))
                            .unwrap_or_default(),
                    }
                }));
            }
            Some("tool_result") => {
                let result_content = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => {
                        let joined = parts
                            .iter()
                            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if joined.is_empty() {
                            serde_json::to_string(block.get("content").unwrap()).unwrap_or_default()
                        } else {
                            joined
                        }
                    }
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                    "content": result_content,
                }));
            }
            _ => {}
        }
    }

    if !tool_results.is_empty() {
        out.extend(tool_results);
        if !text_parts.is_empty() {
            out.push(json!({ "role": "user", "content": text_parts.join("") }));
        }
        return;
    }

    if !tool_calls.is_empty() {
        let mut assistant = json!({ "role": "assistant" });
        if !text_parts.is_empty() {
            assistant["content"] = json!(text_parts.join(""));
        }
        assistant["tool_calls"] = json!(tool_calls);
        out.push(assistant);
        return;
    }

    if !image_parts.is_empty() {
        let mut parts: Vec<Value> = text_parts
            .iter()
            .map(|t| json!({ "type": "text", "text": t }))
            .collect();
        parts.extend(image_parts);
        out.push(json!({ "role": role, "content": parts }));
        return;
    }

    if !text_parts.is_empty() {
        out.push(json!({ "role": role, "content": text_parts.join("") }));
    } else {
        out.push(json!({ "role": role, "content": "" }));
    }
}

fn finish_to_stop_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    }
}

/// Anthropic's `usage.input_tokens` counts only freshly-processed tokens -
/// cache hits are reported separately via `cache_read_input_tokens` - whereas
/// OpenAI's `prompt_tokens` counts everything. Subtract so a client summing
/// `input_tokens + cache_read_input_tokens` gets the same total Codex billed,
/// not a double-count.
fn usage_from(value: &Value) -> Value {
    let Some(u) = value.get("usage") else {
        return json!({ "input_tokens": 0, "output_tokens": 0 });
    };
    let prompt = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let completion = u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
    let mut out = json!({
        "input_tokens": (prompt - cached).max(0),
        "output_tokens": completion
    });
    if cached > 0 {
        out["cache_read_input_tokens"] = json!(cached);
    }
    out
}

/// Aggregated (non-streaming) OpenAI chat.completion JSON -> Anthropic
/// Messages JSON. Note: `transform::aggregate_sse` doesn't reconstruct
/// tool_calls today, so this path only carries text content - acceptable
/// since Claude Code always streams `/v1/messages`.
pub fn openai_json_to_claude_message(value: &Value) -> Value {
    let id = value["id"].as_str().unwrap_or("msg_unknown").to_string();
    let model = value["model"].as_str().unwrap_or("unknown").to_string();
    let choice = &value["choices"][0];
    let content_text = choice["message"]["content"].as_str().unwrap_or("").to_string();
    let finish = choice["finish_reason"].as_str().unwrap_or("stop");

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{ "type": "text", "text": content_text }],
        "stop_reason": finish_to_stop_reason(finish),
        "stop_sequence": null,
        "usage": usage_from(value)
    })
}

#[derive(Default)]
struct ToolCallState {
    block_index: u64,
}

/// Running state for turning a sequence of OpenAI `chat.completion.chunk`
/// events into Anthropic Messages SSE events (block indices, whether the
/// text block is open, per-tool-call block indices).
#[derive(Default)]
pub struct ClaudeStreamState {
    message_start_sent: bool,
    next_block_index: u64,
    text_block_index: u64,
    text_block_started: bool,
    tool_calls: std::collections::BTreeMap<u64, ToolCallState>,
}

impl ClaudeStreamState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Convert one OpenAI `chat.completion.chunk` into zero or more
/// `(event_type, data)` Anthropic Messages SSE events.
pub fn openai_chunk_to_claude_events(
    chunk: &Value,
    state: &mut ClaudeStreamState,
) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let Some(choice) = chunk["choices"].get(0) else {
        return events;
    };
    let delta = &choice["delta"];

    if !state.message_start_sent {
        state.message_start_sent = true;
        let id = chunk["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("msg_unknown")
            .to_string();
        let model = chunk["model"].as_str().unwrap_or("unknown").to_string();
        events.push((
            "message_start".into(),
            json!({
                "type": "message_start",
                "message": {
                    "id": id, "type": "message", "role": "assistant", "model": model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }
            }),
        ));
    }

    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            if !state.text_block_started {
                state.text_block_index = state.next_block_index;
                state.next_block_index += 1;
                state.text_block_started = true;
                events.push((
                    "content_block_start".into(),
                    json!({
                        "type": "content_block_start",
                        "index": state.text_block_index,
                        "content_block": { "type": "text", "text": "" }
                    }),
                ));
            }
            events.push((
                "content_block_delta".into(),
                json!({
                    "type": "content_block_delta",
                    "index": state.text_block_index,
                    "delta": { "type": "text_delta", "text": text }
                }),
            ));
        }
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                if !state.tool_calls.contains_key(&index) {
                    let block_index = state.next_block_index;
                    state.next_block_index += 1;
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    state.tool_calls.insert(index, ToolCallState { block_index });
                    events.push((
                        "content_block_start".into(),
                        json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                        }),
                    ));
                }
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                if let Some(tool) = state.tool_calls.get(&index) {
                    events.push((
                        "content_block_delta".into(),
                        json!({
                            "type": "content_block_delta",
                            "index": tool.block_index,
                            "delta": { "type": "input_json_delta", "partial_json": args }
                        }),
                    ));
                }
            }
        }
    }

    if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        if state.text_block_started {
            events.push((
                "content_block_stop".into(),
                json!({ "type": "content_block_stop", "index": state.text_block_index }),
            ));
        }
        for tool in state.tool_calls.values() {
            events.push((
                "content_block_stop".into(),
                json!({ "type": "content_block_stop", "index": tool.block_index }),
            ));
        }
        events.push((
            "message_delta".into(),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": finish_to_stop_reason(finish), "stop_sequence": null },
                "usage": usage_from(chunk)
            }),
        ));
        events.push(("message_stop".into(), json!({ "type": "message_stop" })));
    }

    events
}

// --- Reverse direction: OpenAI Chat Completions <-> Anthropic Messages, ---
// --- for `passthrough` providers whose own `wire_format` differs from   ---
// --- the client route hit. The two existing adapters above only ever    ---
// --- need Anthropic-client <-> OpenAI-internal, since their upstream    ---
// --- integrations are OpenAI-shape internally; a passthrough provider's ---
// --- own wire_format can be either, so this is the missing half.       ---

fn convert_tool_choice_to_claude(choice: &Value) -> Value {
    match choice {
        Value::String(s) if s == "required" => json!({ "type": "any" }),
        Value::Object(o) if o.get("type").and_then(|t| t.as_str()) == Some("function") => {
            json!({ "type": "tool", "name": o["function"]["name"].clone() })
        }
        _ => json!({ "type": "auto" }),
    }
}

/// OpenAI Chat Completions request body -> Anthropic Messages request body.
pub fn openai_to_claude_request(body: &Value) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(Value::Array(msgs)) = body.get("messages") {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            if role == "system" {
                if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
                    system_parts.push(s.to_string());
                }
                continue;
            }
            convert_openai_message(m, role, &mut messages);
        }
    }

    let mut out = json!({ "messages": messages });
    if !system_parts.is_empty() {
        out["system"] = json!(system_parts.join("\n"));
    }
    if let Some(model) = body.get("model") {
        out["model"] = model.clone();
    }
    if let Some(stream) = body.get("stream") {
        out["stream"] = stream.clone();
    }
    // Anthropic requires max_tokens; OpenAI clients frequently omit it.
    out["max_tokens"] = body.get("max_tokens").cloned().unwrap_or(json!(4096));
    if let Some(temperature) = body.get("temperature") {
        out["temperature"] = temperature.clone();
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        let claude_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                Some(json!({
                    "name": f.get("name").cloned().unwrap_or(json!("")),
                    "description": f.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                    "input_schema": f.get("parameters").cloned()
                        .unwrap_or(json!({"type": "object", "properties": {}})),
                }))
            })
            .collect();
        out["tools"] = json!(claude_tools);
    }
    if let Some(tool_choice) = body.get("tool_choice") {
        out["tool_choice"] = convert_tool_choice_to_claude(tool_choice);
    }

    out
}

fn convert_openai_message(msg: &Value, role: &str, out: &mut Vec<Value>) {
    if role == "tool" {
        out.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": msg.get("tool_call_id").cloned().unwrap_or(json!("")),
                "content": msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string(),
            }]
        }));
        return;
    }

    if role == "assistant" {
        if let Some(Value::Array(tool_calls)) = msg.get("tool_calls") {
            let mut blocks: Vec<Value> = Vec::new();
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            for tc in tool_calls {
                let input: Value = tc["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tc.get("id").cloned().unwrap_or(json!("")),
                    "name": tc["function"]["name"].clone(),
                    "input": input,
                }));
            }
            out.push(json!({ "role": "assistant", "content": blocks }));
            return;
        }
    }

    match msg.get("content").cloned().unwrap_or(Value::Null) {
        Value::String(s) => out.push(json!({ "role": role, "content": s })),
        Value::Array(parts) => {
            let blocks: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") => Some(json!({
                        "type": "text",
                        "text": p.get("text").cloned().unwrap_or(json!(""))
                    })),
                    Some("image_url") => {
                        let url = p["image_url"]["url"].as_str().unwrap_or("");
                        if let Some(rest) = url.strip_prefix("data:") {
                            let (media_type, data) =
                                rest.split_once(";base64,").unwrap_or(("image/png", ""));
                            Some(json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": media_type, "data": data }
                            }))
                        } else {
                            Some(json!({
                                "type": "image",
                                "source": { "type": "url", "url": url }
                            }))
                        }
                    }
                    _ => None,
                })
                .collect();
            out.push(json!({ "role": role, "content": blocks }));
        }
        _ => out.push(json!({ "role": role, "content": "" })),
    }
}

fn stop_reason_to_finish(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// Reverse of `usage_from`: Anthropic's separately-reported cache-read
/// tokens fold back into OpenAI's single `prompt_tokens` total.
fn openai_usage_from_claude(value: &Value) -> Value {
    let Some(u) = value.get("usage") else {
        return json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 });
    };
    let input = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let cached = u
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let prompt = input + cached;
    let mut out = json!({
        "prompt_tokens": prompt,
        "completion_tokens": output,
        "total_tokens": prompt + output,
    });
    if cached > 0 {
        out["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    out
}

/// Aggregated (non-streaming) Anthropic Messages JSON -> OpenAI
/// chat.completion JSON.
pub fn claude_json_to_openai_message(value: &Value) -> Value {
    let id = value["id"].as_str().unwrap_or("msg_unknown").to_string();
    let model = value["model"].as_str().unwrap_or("unknown").to_string();
    let stop_reason = value["stop_reason"].as_str().unwrap_or("end_turn");

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(Value::Array(blocks)) = value.get("content") {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(json!({
                        "id": block.get("id").cloned().unwrap_or(json!("")),
                        "type": "function",
                        "function": {
                            "name": block.get("name").cloned().unwrap_or(json!("")),
                            "arguments": serde_json::to_string(block.get("input").unwrap_or(&json!({})))
                                .unwrap_or_default(),
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let mut message = json!({ "role": "assistant" });
    let finish_reason = if !tool_calls.is_empty() {
        if !text.is_empty() {
            message["content"] = json!(text);
        }
        message["tool_calls"] = json!(tool_calls);
        "tool_calls"
    } else {
        message["content"] = json!(text);
        stop_reason_to_finish(stop_reason)
    };

    json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": openai_usage_from_claude(value)
    })
}

#[derive(Default)]
enum OpenAiBlockKind {
    #[default]
    Text,
    ToolUse,
}

/// Running state for turning a sequence of Anthropic Messages SSE events
/// into OpenAI `chat.completion.chunk` events (per-content-block-index kind,
/// and each tool_use block's OpenAI `tool_calls[].index`).
#[derive(Default)]
pub struct OpenAiStreamState {
    id: String,
    model: String,
    block_kinds: std::collections::BTreeMap<u64, OpenAiBlockKind>,
    tool_openai_index: std::collections::BTreeMap<u64, usize>,
    next_tool_index: usize,
}

impl OpenAiStreamState {
    pub fn new() -> Self {
        Self::default()
    }
}

fn openai_chunk(state: &OpenAiStreamState, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "model": state.model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
}

/// Convert one Anthropic Messages SSE event into zero or one OpenAI
/// `chat.completion.chunk`. Returns `None` for events that carry no
/// client-visible delta on the OpenAI side (`message_stop` in particular -
/// the stream wrapper below turns that into the `[DONE]` marker instead).
pub fn claude_event_to_openai_chunk(
    event_type: &str,
    data: &Value,
    state: &mut OpenAiStreamState,
) -> Option<Value> {
    match event_type {
        "message_start" => {
            state.id = data["message"]["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("msg_unknown")
                .to_string();
            state.model = data["message"]["model"].as_str().unwrap_or("unknown").to_string();
            Some(openai_chunk(state, json!({ "role": "assistant", "content": "" }), None))
        }
        "content_block_start" => {
            let index = data["index"].as_u64().unwrap_or(0);
            match data["content_block"]["type"].as_str() {
                Some("tool_use") => {
                    state.block_kinds.insert(index, OpenAiBlockKind::ToolUse);
                    let openai_index = state.next_tool_index;
                    state.next_tool_index += 1;
                    state.tool_openai_index.insert(index, openai_index);
                    let id = data["content_block"]["id"].clone();
                    let name = data["content_block"]["name"].clone();
                    Some(openai_chunk(
                        state,
                        json!({ "tool_calls": [{
                            "index": openai_index, "id": id, "type": "function",
                            "function": { "name": name, "arguments": "" }
                        }] }),
                        None,
                    ))
                }
                _ => {
                    state.block_kinds.insert(index, OpenAiBlockKind::Text);
                    None
                }
            }
        }
        "content_block_delta" => {
            let index = data["index"].as_u64().unwrap_or(0);
            match state.block_kinds.get(&index) {
                Some(OpenAiBlockKind::ToolUse) => {
                    let openai_index = *state.tool_openai_index.get(&index).unwrap_or(&0);
                    let args = data["delta"]["partial_json"].as_str().unwrap_or("");
                    Some(openai_chunk(
                        state,
                        json!({ "tool_calls": [{
                            "index": openai_index,
                            "function": { "arguments": args }
                        }] }),
                        None,
                    ))
                }
                _ => {
                    let text = data["delta"]["text"].as_str().unwrap_or("");
                    if text.is_empty() {
                        None
                    } else {
                        Some(openai_chunk(state, json!({ "content": text }), None))
                    }
                }
            }
        }
        "message_delta" => {
            let stop_reason = data["delta"]["stop_reason"].as_str().unwrap_or("end_turn");
            let finish = if !state.tool_openai_index.is_empty() {
                "tool_calls"
            } else {
                stop_reason_to_finish(stop_reason)
            };
            let mut chunk = openai_chunk(state, json!({}), Some(finish));
            if data.get("usage").is_some() {
                chunk["usage"] = openai_usage_from_claude(&json!({ "usage": data["usage"] }));
            }
            Some(chunk)
        }
        _ => None,
    }
}

/// Reframe an arbitrarily-chunked SSE byte stream (network reads don't
/// align to `\n\n`-terminated block boundaries) into one item per complete
/// block, still ending in the blank-line separator. Needed before either
/// `convert_openai_sse_to_claude_sse` or `convert_claude_sse_to_openai_sse`
/// when the upstream bytes come straight from a real HTTP response
/// (`HttpAdapter`) rather than from the Codex/Command Code adapters'
/// own envelope-translating stream, which already reframes as a side
/// effect of that translation.
pub fn reframe_sse_blocks<S, E>(upstream: S) -> impl futures::Stream<Item = Result<Bytes, E>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    struct St<S> {
        upstream: S,
        buf: String,
        upstream_done: bool,
    }

    let st = St {
        upstream,
        buf: String::new(),
        upstream_done: false,
    };

    futures::stream::unfold(st, |mut st| async move {
        use futures::StreamExt;
        loop {
            if let Some(pos) = st.buf.find("\n\n") {
                let block: String = st.buf.drain(..pos + 2).collect();
                if block.trim().is_empty() {
                    continue;
                }
                return Some((Ok(Bytes::from(block)), st));
            }
            if st.upstream_done {
                if st.buf.trim().is_empty() {
                    return None;
                }
                let rest = std::mem::take(&mut st.buf);
                return Some((Ok(Bytes::from(rest)), st));
            }
            match st.upstream.next().await {
                Some(Ok(bytes)) => st.buf.push_str(&String::from_utf8_lossy(&bytes)),
                Some(Err(e)) => {
                    st.upstream_done = true;
                    return Some((Err(e), st));
                }
                None => st.upstream_done = true,
            }
        }
    })
}

/// Parse one Anthropic Messages SSE block (`event: X\ndata: Y`, arbitrary
/// line order/extra fields tolerated) into `(event_type, data)`.
fn parse_claude_sse_block(block: &str) -> Option<(String, Value)> {
    let mut event_type = None;
    let mut data_line = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_line = Some(rest.trim().to_string());
        }
    }
    let data: Value = serde_json::from_str(&data_line?).ok()?;
    Some((event_type.unwrap_or_default(), data))
}

/// Turn a stream of already-framed Anthropic Messages `event:
/// ...\ndata: {...}\n\n` SSE blocks into a stream of OpenAI
/// `chat.completion.chunk` `data: {...}\n\n` SSE blocks, appending
/// `data: [DONE]\n\n` once `message_stop` is seen or upstream ends -
/// Anthropic streams have no `[DONE]` marker; OpenAI's do.
pub fn convert_claude_sse_to_openai_sse<S, E>(
    upstream: S,
) -> impl futures::Stream<Item = Result<Bytes, E>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    use std::pin::Pin;

    struct St<E> {
        upstream: Pin<Box<dyn futures::Stream<Item = Result<Bytes, E>> + Send>>,
        state: OpenAiStreamState,
        stopped: bool,
    }

    let st = St {
        upstream: Box::pin(upstream),
        state: OpenAiStreamState::new(),
        stopped: false,
    };

    futures::stream::unfold(st, |mut st| async move {
        use futures::StreamExt;
        loop {
            if st.stopped {
                return None;
            }
            match st.upstream.next().await {
                Some(Ok(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let Some((event_type, data)) = parse_claude_sse_block(&text) else {
                        continue;
                    };
                    if event_type == "message_stop" {
                        st.stopped = true;
                        return Some((Ok(Bytes::from_static(b"data: [DONE]\n\n")), st));
                    }
                    let Some(chunk) = claude_event_to_openai_chunk(&event_type, &data, &mut st.state)
                    else {
                        continue;
                    };
                    return Some((Ok(Bytes::from(format!("data: {chunk}\n\n"))), st));
                }
                Some(Err(e)) => {
                    st.stopped = true;
                    return Some((Err(e), st));
                }
                None => {
                    st.stopped = true;
                    return Some((Ok(Bytes::from_static(b"data: [DONE]\n\n")), st));
                }
            }
        }
    })
}

pub fn render_claude_event(event_type: &str, data: &Value) -> Vec<u8> {
    format!("event: {event_type}\ndata: {data}\n\n").into_bytes()
}

/// Turn a stream of already-framed OpenAI `data: {...}\n\n` SSE chunks (as
/// produced by `transform::convert_sse_stream`, one complete block per item,
/// terminated by a `data: [DONE]\n\n` marker) into a stream of Anthropic
/// Messages `event: ...\ndata: {...}\n\n` SSE blocks. Anthropic streams have
/// no `[DONE]` marker - the `[DONE]` item is consumed and dropped.
pub fn convert_openai_sse_to_claude_sse<S, E>(
    upstream: S,
) -> impl futures::Stream<Item = Result<Bytes, E>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    use std::collections::VecDeque;
    use std::pin::Pin;

    struct St<E> {
        // Boxed+pinned so callers don't need to prove `S: Unpin` - the
        // concrete stream returned by `convert_sse_stream` isn't guaranteed
        // to be.
        upstream: Pin<Box<dyn futures::Stream<Item = Result<Bytes, E>> + Send>>,
        state: ClaudeStreamState,
        stopped: bool,
    }

    let st = St {
        upstream: Box::pin(upstream),
        state: ClaudeStreamState::new(),
        stopped: false,
    };

    futures::stream::unfold((st, VecDeque::<Bytes>::new()), |(mut st, mut queue)| async move {
        use futures::StreamExt;
        loop {
            if let Some(bytes) = queue.pop_front() {
                return Some((Ok(bytes), (st, queue)));
            }
            if st.stopped {
                return None;
            }
            match st.upstream.next().await {
                Some(Ok(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let Some(rest) = text.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let data_line = rest.trim();
                    if data_line == "[DONE]" {
                        st.stopped = true;
                        continue;
                    }
                    let Ok(chunk) = serde_json::from_str::<Value>(data_line) else {
                        continue;
                    };
                    for (event_type, data) in openai_chunk_to_claude_events(&chunk, &mut st.state) {
                        queue.push_back(Bytes::from(render_claude_event(&event_type, &data)));
                    }
                    continue;
                }
                Some(Err(e)) => {
                    st.stopped = true;
                    return Some((Err(e), (st, queue)));
                }
                None => {
                    st.stopped = true;
                    continue;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_request_converts_system_and_text_message() {
        let input = json!({
            "model": "gpt-5-codex",
            "system": "be nice",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let out = claude_to_openai_request(&input);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be nice");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn claude_request_converts_tool_use_and_tool_result() {
        let input = json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "sf"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"}
                ]}
            ]
        });
        let out = claude_to_openai_request(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_1");
        assert_eq!(msgs[1]["content"], "sunny");
    }

    #[test]
    fn claude_request_converts_tools_and_tool_choice() {
        let input = json!({
            "messages": [],
            "tools": [{"name": "get_weather", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "get_weather"}
        });
        let out = claude_to_openai_request(&input);
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["function"]["name"], "get_weather");
    }

    #[test]
    fn stream_events_emit_message_start_then_text_delta() {
        let mut state = ClaudeStreamState::new();
        let chunk = json!({
            "id": "resp_1", "model": "gpt-5-codex",
            "choices": [{"delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
        });
        let events = openai_chunk_to_claude_events(&chunk, &mut state);
        assert_eq!(events[0].0, "message_start");
        assert_eq!(events[0].1["message"]["id"], "resp_1");
        // The Anthropic SDK/Claude Code discriminates events by data.type,
        // not the SSE event: line. Regression for "Stream completed without
        // receiving message_start event".
        assert_eq!(events[0].1["type"], "message_start");

        let chunk2 = json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]});
        let events2 = openai_chunk_to_claude_events(&chunk2, &mut state);
        assert_eq!(events2[0].0, "content_block_start");
        assert_eq!(events2[0].1["type"], "content_block_start");
        assert_eq!(events2[1].0, "content_block_delta");
        assert_eq!(events2[1].1["type"], "content_block_delta");
        assert_eq!(events2[1].1["delta"]["text"], "hi");
    }

    #[test]
    fn stream_events_emit_tool_use_block() {
        let mut state = ClaudeStreamState::new();
        state.message_start_sent = true;
        let chunk = json!({
            "choices": [{
                "delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "get_weather", "arguments": ""}}]},
                "finish_reason": null
            }]
        });
        let events = openai_chunk_to_claude_events(&chunk, &mut state);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["type"], "content_block_start");
        assert_eq!(events[0].1["content_block"]["type"], "tool_use");
        assert_eq!(events[0].1["content_block"]["id"], "call_1");

        let chunk2 = json!({
            "choices": [{
                "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\""}}]},
                "finish_reason": null
            }]
        });
        let events2 = openai_chunk_to_claude_events(&chunk2, &mut state);
        assert_eq!(events2[0].0, "content_block_delta");
        assert_eq!(events2[0].1["type"], "content_block_delta");
        assert_eq!(events2[0].1["delta"]["partial_json"], "{\"city\"");
    }

    #[test]
    fn stream_events_finish_closes_blocks_and_emits_stop() {
        let mut state = ClaudeStreamState::new();
        state.message_start_sent = true;
        state.text_block_started = true;
        state.text_block_index = 0;
        state.next_block_index = 1;
        let chunk = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        let events = openai_chunk_to_claude_events(&chunk, &mut state);
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["type"], "content_block_stop");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[1].0, "message_delta");
        assert_eq!(events[1].1["type"], "message_delta");
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[2].0, "message_stop");
        assert_eq!(events[2].1["type"], "message_stop");
    }

    #[test]
    fn stream_finish_reports_cache_read_tokens_and_deducts_them_from_input() {
        let mut state = ClaudeStreamState::new();
        state.message_start_sent = true;
        let chunk = json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 10,
                "prompt_tokens_details": { "cached_tokens": 80 }
            }
        });
        let events = openai_chunk_to_claude_events(&chunk, &mut state);
        let usage = &events[0].1["usage"];
        assert_eq!(usage["input_tokens"], 20);
        assert_eq!(usage["output_tokens"], 10);
        assert_eq!(usage["cache_read_input_tokens"], 80);
    }

    #[test]
    fn tool_calls_finish_reason_maps_to_tool_use_stop_reason() {
        let mut state = ClaudeStreamState::new();
        state.message_start_sent = true;
        let chunk = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
        let events = openai_chunk_to_claude_events(&chunk, &mut state);
        assert_eq!(events[0].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn non_streaming_json_converts_to_claude_message() {
        let input = json!({
            "id": "resp_1", "model": "gpt-5-codex",
            "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}]
        });
        let out = openai_json_to_claude_message(&input);
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn non_streaming_json_reports_cache_read_tokens() {
        let input = json!({
            "id": "resp_1", "model": "gpt-5-codex",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 50, "completion_tokens": 5,
                "prompt_tokens_details": { "cached_tokens": 40 }
            }
        });
        let out = openai_json_to_claude_message(&input);
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 5);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 40);
    }

    fn block_from(bytes_chunks: Vec<&str>) -> Vec<Result<Bytes, std::io::Error>> {
        bytes_chunks
            .into_iter()
            .map(|s| Ok(Bytes::from(s.to_string())))
            .collect()
    }

    #[test]
    fn convert_openai_sse_to_claude_sse_translates_and_drops_done_marker() {
        let openai_sse = vec![
            "data: {\"id\":\"resp_1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let upstream = futures::stream::iter(block_from(openai_sse));
        let converted = convert_openai_sse_to_claude_sse(upstream);
        let out: Vec<Result<Bytes, std::io::Error>> =
            futures::executor::block_on(futures::StreamExt::collect::<Vec<_>>(converted));
        let rendered: String = out
            .into_iter()
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect();

        assert!(rendered.contains("event: message_start"));
        assert!(rendered.contains("event: content_block_start"));
        assert!(rendered.contains("\"text\":\"Hi\""));
        assert!(rendered.contains("event: message_stop"));
        assert!(!rendered.contains("[DONE]"));
    }

    #[test]
    fn openai_request_extracts_system_message() {
        let input = json!({
            "model": "claude-x",
            "messages": [
                {"role": "system", "content": "be nice"},
                {"role": "user", "content": "hi"}
            ]
        });
        let out = openai_to_claude_request(&input);
        assert_eq!(out["system"], "be nice");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "hi");
        assert_eq!(out["max_tokens"], 4096, "defaults when the OpenAI body omits it");
    }

    #[test]
    fn openai_request_converts_tool_call_and_tool_result() {
        let input = json!({
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"sf\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        });
        let out = openai_to_claude_request(&input);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[0]["content"][0]["id"], "call_1");
        assert_eq!(msgs[0]["content"][0]["input"]["city"], "sf");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(msgs[1]["content"][0]["content"], "sunny");
    }

    #[test]
    fn openai_request_converts_tools_and_tool_choice() {
        let input = json!({
            "messages": [],
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "d", "parameters": {"type": "object"}
            }}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}}
        });
        let out = openai_to_claude_request(&input);
        assert_eq!(out["tools"][0]["name"], "get_weather");
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(out["tool_choice"]["type"], "tool");
        assert_eq!(out["tool_choice"]["name"], "get_weather");
    }

    #[test]
    fn claude_json_converts_to_openai_message() {
        let input = json!({
            "id": "msg_1", "model": "claude-x", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 4}
        });
        let out = claude_json_to_openai_message(&input);
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 14);
        assert_eq!(out["usage"]["completion_tokens"], 5);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
    }

    #[test]
    fn claude_json_converts_tool_use_to_tool_calls() {
        let input = json!({
            "id": "msg_1", "model": "claude-x", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "sf"}}]
        });
        let out = claude_json_to_openai_message(&input);
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn claude_stream_events_emit_role_then_text_delta() {
        let mut state = OpenAiStreamState::new();
        let start = json!({"message": {"id": "msg_1", "model": "claude-x"}});
        let chunk = claude_event_to_openai_chunk("message_start", &start, &mut state).unwrap();
        assert_eq!(chunk["id"], "msg_1");
        assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");

        claude_event_to_openai_chunk(
            "content_block_start",
            &json!({"index": 0, "content_block": {"type": "text"}}),
            &mut state,
        );
        let delta_chunk = claude_event_to_openai_chunk(
            "content_block_delta",
            &json!({"index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            &mut state,
        )
        .unwrap();
        assert_eq!(delta_chunk["choices"][0]["delta"]["content"], "hi");
    }

    #[test]
    fn claude_stream_events_emit_tool_use_block() {
        let mut state = OpenAiStreamState::new();
        let start_chunk = claude_event_to_openai_chunk(
            "content_block_start",
            &json!({"index": 0, "content_block": {"type": "tool_use", "id": "call_1", "name": "get_weather"}}),
            &mut state,
        )
        .unwrap();
        assert_eq!(start_chunk["choices"][0]["delta"]["tool_calls"][0]["id"], "call_1");
        assert_eq!(start_chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "get_weather");

        let delta_chunk = claude_event_to_openai_chunk(
            "content_block_delta",
            &json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\""}}),
            &mut state,
        )
        .unwrap();
        assert_eq!(delta_chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"city\"");
    }

    #[test]
    fn claude_stream_message_delta_emits_finish_and_usage() {
        let mut state = OpenAiStreamState::new();
        let data = json!({
            "delta": {"stop_reason": "max_tokens"},
            "usage": {"input_tokens": 20, "output_tokens": 3}
        });
        let chunk = claude_event_to_openai_chunk("message_delta", &data, &mut state).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "length");
        assert_eq!(chunk["usage"]["prompt_tokens"], 20);
        assert_eq!(chunk["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn reframe_sse_blocks_reassembles_block_split_across_chunks() {
        let chunks = block_from(vec!["event: message_start\ndata: {\"a\":", "1}\n\n"]);
        let out: Vec<Result<Bytes, std::io::Error>> = futures::executor::block_on(
            futures::StreamExt::collect::<Vec<_>>(reframe_sse_blocks(futures::stream::iter(chunks))),
        );
        assert_eq!(out.len(), 1);
        let text = String::from_utf8(out[0].as_ref().unwrap().to_vec()).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("{\"a\":1}"));
    }

    #[test]
    fn reframe_sse_blocks_splits_multiple_blocks_in_one_chunk() {
        let chunks = block_from(vec!["data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"]);
        let out: Vec<Result<Bytes, std::io::Error>> = futures::executor::block_on(
            futures::StreamExt::collect::<Vec<_>>(reframe_sse_blocks(futures::stream::iter(chunks))),
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn convert_claude_sse_to_openai_sse_translates_and_appends_done() {
        let claude_sse = vec![
            "event: message_start\ndata: {\"message\":{\"id\":\"msg_1\",\"model\":\"claude-x\"}}\n\n",
            "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {}\n\n",
        ];
        let upstream = futures::stream::iter(block_from(claude_sse));
        let converted = convert_claude_sse_to_openai_sse(upstream);
        let out: Vec<Result<Bytes, std::io::Error>> =
            futures::executor::block_on(futures::StreamExt::collect::<Vec<_>>(converted));
        let rendered: String = out
            .into_iter()
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect();

        assert!(rendered.contains("\"role\":\"assistant\""));
        assert!(rendered.contains("\"content\":\"Hi\""));
        assert!(rendered.contains("\"finish_reason\":\"stop\""));
        assert!(rendered.contains("[DONE]"));
    }
}
