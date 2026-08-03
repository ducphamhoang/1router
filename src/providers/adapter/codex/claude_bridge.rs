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
                        "index": state.text_block_index,
                        "content_block": { "type": "text", "text": "" }
                    }),
                ));
            }
            events.push((
                "content_block_delta".into(),
                json!({
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
            events.push(("content_block_stop".into(), json!({ "index": state.text_block_index })));
        }
        for tool in state.tool_calls.values() {
            events.push(("content_block_stop".into(), json!({ "index": tool.block_index })));
        }
        events.push((
            "message_delta".into(),
            json!({
                "delta": { "stop_reason": finish_to_stop_reason(finish), "stop_sequence": null },
                "usage": usage_from(chunk)
            }),
        ));
        events.push(("message_stop".into(), json!({})));
    }

    events
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

        let chunk2 = json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]});
        let events2 = openai_chunk_to_claude_events(&chunk2, &mut state);
        assert_eq!(events2[0].0, "content_block_start");
        assert_eq!(events2[1].0, "content_block_delta");
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
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[1].0, "message_delta");
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[2].0, "message_stop");
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
}
