use bytes::Bytes;
use serde_json::{json, Value};

// Fields Codex's backend rejects. This is a denylist rather than the spec's
// ideal "strict allowlist" (keep-known-fields), which means any future
// OpenAI-SDK field not covered here leaks through by default - but a true
// allowlist would need to enumerate everything a real client legitimately
// sends, which risks silently dropping fields Codex *does* support. Extended
// per the Phase 3 review with the other common Chat Completions params real
// clients (Cursor, Claude Code, the OpenAI SDK) routinely send.
const DISALLOWED: &[&str] = &[
    "temperature",
    "top_p",
    "max_tokens",
    "max_output_tokens",
    "user",
    "n",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "seed",
    "stop",
    "response_format",
    "stream_options",
    "parallel_tool_calls",
    "service_tier",
];

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn strip_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("id");
            for (_, v) in map.iter_mut() {
                strip_ids(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_ids(v);
            }
        }
        _ => {}
    }
}

fn flatten_function_shape(value: &mut Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if map.get("type").and_then(|t| t.as_str()) != Some("function") {
        return;
    }
    if let Some(Value::Object(function)) = map.remove("function") {
        for (k, v) in function {
            map.insert(k, v);
        }
    }
}

pub fn transform_request(client_json: &Value, session_id: &str) -> Value {
    let mut out = client_json.clone();
    let obj = match out.as_object_mut() {
        Some(o) => o,
        None => return out,
    };

    // Strict allowlist: delete fields Codex's backend rejects.
    for key in DISALLOWED {
        obj.remove(*key);
    }

    if let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in msgs.iter_mut() {
            if m.get("role").and_then(|r| r.as_str()) == Some("system") {
                m["role"] = json!("developer");
            }
        }
    }

    // The Responses API takes `input`, not Chat Completions' `messages` - real
    // OpenAI-SDK clients send `messages`, and forwarding that field unconverted
    // gets a 400 from the real backend (confirmed via a real-account Phase 4
    // e2e run; the design spec had left this shape unconfirmed).
    if let Some(Value::Array(messages)) = obj.remove("messages") {
        if !obj.contains_key("input") {
            let input: Vec<Value> = messages
                .into_iter()
                .map(|m| {
                    let role = m
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("user")
                        .to_string();
                    let text = message_text(&m);
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    json!({
                        "type": "message",
                        "role": role,
                        "content": [{ "type": part_type, "text": text }]
                    })
                })
                .collect();
            obj.insert("input".into(), json!(input));
        }
    }

    if let Some(input) = obj.get_mut("input") {
        strip_ids(input);
    }

    // Chat Completions nests function specs under tools[i].function.{name,
    // description,parameters}; the Responses API expects them flattened onto
    // tools[i] directly. Forwarding the nested shape unchanged makes the
    // Responses backend report the flattened field as missing (e.g.
    // "tools[0].name"), even though the client did send a name.
    if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools.iter_mut() {
            flatten_function_shape(tool);
        }
    }
    if let Some(tool_choice) = obj.get_mut("tool_choice") {
        flatten_function_shape(tool_choice);
    }

    obj.insert("store".into(), json!(false));
    obj.insert("stream".into(), json!(true));
    obj.insert("prompt_cache_key".into(), json!(session_id));

    let reasoning = obj.entry("reasoning").or_insert_with(|| json!({}));
    if reasoning.get("effort").is_none() {
        reasoning["effort"] = json!("medium");
    }
    obj.insert("include".into(), json!(["reasoning.encrypted_content"]));

    out
}

/// Parse one SSE block (the text between two blank lines, no trailing
/// newlines) into its (event, data-json) pair, if it carries a `data:` line
/// with valid JSON.
pub fn parse_sse_block(block: &str) -> Option<(String, Value)> {
    let mut event = String::new();
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim());
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data).ok().map(|j| (event, j))
}

/// Parse an SSE body into (event, data-json) pairs.
fn sse_events(sse_body: &str) -> Vec<(String, Value)> {
    sse_body.split("\n\n").filter_map(parse_sse_block).collect()
}

/// Turn a Responses-API `response.usage` object into a Chat-Completions-shaped
/// `usage` value, carrying `prompt_tokens_details.cached_tokens` through so
/// downstream (claude_bridge) can report real cache-hit counts instead of
/// silently dropping them - the Responses API's own automatic prefix caching
/// happens regardless of which client wire format a request came in on;
/// this only affects whether we *report* it.
fn chat_completions_usage(usage: &Value) -> Option<Value> {
    let usage = usage.as_object()?;
    let prompt = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let completion = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut out = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion
    });
    if cached > 0 {
        out["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    Some(out)
}

pub fn aggregate_sse(sse_body: &str, model: &str) -> Value {
    let mut content = String::new();
    let mut resp_id = String::new();
    let mut usage: Option<Value> = None;
    for (event, data) in sse_events(sse_body) {
        if event.ends_with("output_text.delta") {
            if let Some(d) = data["delta"].as_str() {
                content.push_str(d);
            }
        } else if event.ends_with("completed") {
            if let Some(id) = data["response"]["id"].as_str() {
                resp_id = id.to_string();
            }
            usage = chat_completions_usage(&data["response"]["usage"]);
        }
    }
    let mut out = json!({
        "id": resp_id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    });
    if let Some(u) = usage {
        out["usage"] = u;
    }
    out
}

pub fn sse_embedded_error(sse_body: &str) -> Option<String> {
    for (event, data) in sse_events(sse_body) {
        if event.contains("failed") || event.contains("error") || !data["error"].is_null() {
            let t = data["error"]["type"].as_str().unwrap_or("upstream_error");
            return Some(t.to_string());
        }
    }
    None
}

/// Running state needed to turn a sequence of Responses-API SSE events into
/// Chat Completions `chat.completion.chunk` events - carries the response id
/// (only known once `response.created` arrives) and whether any tool call
/// was seen, which decides the final `finish_reason`.
#[derive(Default)]
pub struct SseChunkState {
    id: String,
    created: i64,
    saw_tool_call: bool,
}

impl SseChunkState {
    pub fn new() -> Self {
        Self::default()
    }
}

fn chat_chunk(state: &SseChunkState, model: &str, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": state.created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }]
    })
}

/// Convert one parsed Responses-API SSE event into a Chat Completions
/// streaming chunk, mutating `state` as needed (id/created captured from
/// `response.created`, tool-call flag set from `response.output_item.added`).
/// Returns `None` for upstream events that have no Chat Completions
/// equivalent (e.g. `response.in_progress`).
pub fn chat_chunk_for_event(
    state: &mut SseChunkState,
    event: &str,
    data: &Value,
    model: &str,
) -> Option<Value> {
    match event {
        "response.created" => {
            state.id = data["response"]["id"].as_str().unwrap_or_default().to_string();
            state.created = data["response"]["created_at"].as_i64().unwrap_or(0);
            Some(chat_chunk(
                state,
                model,
                json!({ "role": "assistant", "content": "" }),
                None,
            ))
        }
        "response.output_text.delta" => {
            let text = data["delta"].as_str().unwrap_or_default();
            Some(chat_chunk(state, model, json!({ "content": text }), None))
        }
        "response.output_item.added" => {
            let item = &data["item"];
            if item["type"].as_str() != Some("function_call") {
                return None;
            }
            state.saw_tool_call = true;
            let index = data["output_index"].as_u64().unwrap_or(0);
            let call_id = item["call_id"].as_str().unwrap_or_default();
            let name = item["name"].as_str().unwrap_or_default();
            Some(chat_chunk(
                state,
                model,
                json!({
                    "tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" }
                    }]
                }),
                None,
            ))
        }
        "response.function_call_arguments.delta" => {
            let index = data["output_index"].as_u64().unwrap_or(0);
            let delta = data["delta"].as_str().unwrap_or_default();
            Some(chat_chunk(
                state,
                model,
                json!({
                    "tool_calls": [{
                        "index": index,
                        "function": { "arguments": delta }
                    }]
                }),
                None,
            ))
        }
        "response.completed" => {
            let finish = if state.saw_tool_call { "tool_calls" } else { "stop" };
            let mut chunk = chat_chunk(state, model, json!({}), Some(finish));
            if let Some(usage) = chat_completions_usage(&data["response"]["usage"]) {
                chunk["usage"] = usage;
            }
            Some(chunk)
        }
        _ => None,
    }
}

pub fn render_chunk(chunk: &Value) -> Vec<u8> {
    format!("data: {chunk}\n\n").into_bytes()
}

pub const SSE_DONE: &[u8] = b"data: [DONE]\n\n";

/// Turn a byte stream of Responses-API SSE (arbitrarily chunked - network
/// reads don't align to SSE block boundaries) into a byte stream of Chat
/// Completions SSE, terminated with a `[DONE]` marker once upstream ends.
pub fn convert_sse_stream<S, E>(
    upstream: S,
    model: String,
) -> impl futures::Stream<Item = Result<Bytes, E>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    struct State<S> {
        upstream: S,
        buf: String,
        chunk_state: SseChunkState,
        model: String,
        finished: bool,
    }

    let state = State {
        upstream,
        buf: String::new(),
        chunk_state: SseChunkState::new(),
        model,
        finished: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        use futures::StreamExt;
        loop {
            if st.finished {
                return None;
            }
            if let Some(pos) = st.buf.find("\n\n") {
                let block: String = st.buf.drain(..pos + 2).collect();
                let block = block.trim_end_matches("\n\n").to_string();
                let Some((event, data)) = parse_sse_block(&block) else {
                    continue;
                };
                let Some(chunk) = chat_chunk_for_event(&mut st.chunk_state, &event, &data, &st.model)
                else {
                    continue;
                };
                return Some((Ok(Bytes::from(render_chunk(&chunk))), st));
            }
            match st.upstream.next().await {
                Some(Ok(bytes)) => {
                    st.buf.push_str(&String::from_utf8_lossy(&bytes));
                    continue;
                }
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
                None => {
                    st.finished = true;
                    return Some((Ok(Bytes::from_static(SSE_DONE)), st));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allowlist_deletes_disallowed_fields() {
        let input = json!({
            "model": "gpt-4o",
            "messages": [{"role": "system", "content": "be nice"}],
            "temperature": 0.7, "top_p": 0.9, "max_tokens": 100,
            "max_output_tokens": 50, "user": "u1"
        });
        let out = transform_request(&input, "sess-1");
        assert!(out.get("temperature").is_none());
        assert!(out.get("top_p").is_none());
        assert!(out.get("max_tokens").is_none());
        assert!(out.get("max_output_tokens").is_none());
        assert!(out.get("user").is_none());
    }

    #[test]
    fn allowlist_deletes_common_openai_sdk_fields() {
        // Regression test for the Phase 3 review: real clients (Cursor, Claude
        // Code, the OpenAI SDK) routinely send these too, and Codex rejects them.
        let input = json!({
            "model": "gpt-4o", "messages": [],
            "n": 1, "presence_penalty": 0.1, "frequency_penalty": 0.1,
            "logprobs": true, "top_logprobs": 3, "logit_bias": {"50256": -100},
            "seed": 42, "stop": ["\n"], "response_format": {"type": "json_object"},
            "stream_options": {"include_usage": true}, "parallel_tool_calls": false,
            "service_tier": "default"
        });
        let out = transform_request(&input, "sess-2");
        for field in [
            "n", "presence_penalty", "frequency_penalty", "logprobs", "top_logprobs",
            "logit_bias", "seed", "stop", "response_format", "stream_options",
            "parallel_tool_calls", "service_tier",
        ] {
            assert!(out.get(field).is_none(), "{field} should have been stripped");
        }
    }

    #[test]
    fn system_role_becomes_developer() {
        let input = json!({ "messages": [{"role": "system", "content": "x"}] });
        let out = transform_request(&input, "s");
        assert!(out.get("messages").is_none());
        assert_eq!(out["input"][0]["role"], "developer");
    }

    #[test]
    fn messages_convert_to_responses_input() {
        let input = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello there"}
            ]
        });
        let out = transform_request(&input, "s");
        assert!(out.get("messages").is_none(), "messages should be removed");
        let items = out["input"].as_array().unwrap();
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hi");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["content"][0]["text"], "hello there");
    }

    #[test]
    fn existing_input_field_is_not_overwritten_by_messages_conversion() {
        let input = json!({ "messages": [], "input": [{"id": "msg_abc", "type": "message"}] });
        let out = transform_request(&input, "s");
        assert!(out.get("messages").is_none());
        assert!(out["input"][0].get("id").is_none());
    }

    #[test]
    fn forces_store_false_stream_true_and_cache_key() {
        let input = json!({ "messages": [], "stream": false, "store": true });
        let out = transform_request(&input, "sess-9");
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert_eq!(out["prompt_cache_key"], "sess-9");
        assert_eq!(out["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn tools_flatten_from_chat_completions_to_responses_shape() {
        let input = json!({
            "messages": [],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]
        });
        let out = transform_request(&input, "s");
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_weather");
        assert_eq!(tool["description"], "Get the weather");
        assert_eq!(tool["parameters"]["type"], "object");
        assert!(tool.get("function").is_none());
    }

    #[test]
    fn tool_choice_function_variant_flattens_too() {
        let input = json!({
            "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}}
        });
        let out = transform_request(&input, "s");
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["name"], "get_weather");
        assert!(out["tool_choice"].get("function").is_none());
    }

    #[test]
    fn tool_choice_string_variant_is_untouched() {
        let input = json!({ "messages": [], "tool_choice": "auto" });
        let out = transform_request(&input, "s");
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn strips_item_ids() {
        let input = json!({ "messages": [], "input": [{"id": "msg_abc", "type": "message"}] });
        let out = transform_request(&input, "s");
        assert!(out["input"][0].get("id").is_none());
    }

    #[test]
    fn chat_chunk_for_created_event_carries_id_and_role() {
        let mut state = SseChunkState::new();
        let data = json!({"response": {"id": "resp_1", "created_at": 1785055543}});
        let chunk = chat_chunk_for_event(&mut state, "response.created", &data, "gpt-5.4").unwrap();
        assert_eq!(chunk["id"], "resp_1");
        assert_eq!(chunk["created"], 1785055543);
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");
        assert!(chunk["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn chat_chunk_for_text_delta_carries_content() {
        let mut state = SseChunkState::new();
        let data = json!({"delta": "Hello"});
        let chunk =
            chat_chunk_for_event(&mut state, "response.output_text.delta", &data, "m").unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["content"], "Hello");
    }

    #[test]
    fn chat_chunk_for_function_call_added_emits_tool_call_header() {
        let mut state = SseChunkState::new();
        let data = json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "get_weather"}
        });
        let chunk =
            chat_chunk_for_event(&mut state, "response.output_item.added", &data, "m").unwrap();
        let tc = &chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], "");
        assert!(state.saw_tool_call);
    }

    #[test]
    fn chat_chunk_for_message_item_added_is_ignored() {
        let mut state = SseChunkState::new();
        let data = json!({"output_index": 0, "item": {"type": "message"}});
        assert!(chat_chunk_for_event(&mut state, "response.output_item.added", &data, "m")
            .is_none());
        assert!(!state.saw_tool_call);
    }

    #[test]
    fn chat_chunk_for_function_call_arguments_delta_carries_index_and_partial_args() {
        let mut state = SseChunkState::new();
        let data = json!({"output_index": 2, "delta": "{\"loc"});
        let chunk = chat_chunk_for_event(
            &mut state,
            "response.function_call_arguments.delta",
            &data,
            "m",
        )
        .unwrap();
        let tc = &chunk["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 2);
        assert_eq!(tc["function"]["arguments"], "{\"loc");
        assert!(tc.get("id").is_none());
    }

    #[test]
    fn chat_chunk_for_completed_sets_finish_reason_stop_without_tool_calls() {
        let mut state = SseChunkState::new();
        let chunk = chat_chunk_for_event(&mut state, "response.completed", &json!({}), "m").unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn chat_chunk_for_completed_sets_finish_reason_tool_calls_when_tool_call_seen() {
        let mut state = SseChunkState::new();
        state.saw_tool_call = true;
        let chunk = chat_chunk_for_event(&mut state, "response.completed", &json!({}), "m").unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn chat_chunk_for_completed_carries_cached_token_count() {
        let mut state = SseChunkState::new();
        let data = json!({
            "response": {
                "usage": {
                    "input_tokens": 100, "output_tokens": 10,
                    "input_tokens_details": { "cached_tokens": 80 }
                }
            }
        });
        let chunk = chat_chunk_for_event(&mut state, "response.completed", &data, "m").unwrap();
        assert_eq!(chunk["usage"]["prompt_tokens"], 100);
        assert_eq!(chunk["usage"]["prompt_tokens_details"]["cached_tokens"], 80);
    }

    #[test]
    fn aggregate_sse_carries_cached_token_count() {
        let sse = "event: response.completed\n\
                   data: {\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":50,\"output_tokens\":5,\
                   \"input_tokens_details\":{\"cached_tokens\":40}}}}\n\n";
        let out = aggregate_sse(sse, "m");
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 40);
    }

    #[test]
    fn chat_chunk_ignores_unmapped_events() {
        let mut state = SseChunkState::new();
        assert!(chat_chunk_for_event(&mut state, "response.in_progress", &json!({}), "m").is_none());
    }

    fn block_from(bytes_chunks: Vec<&str>) -> Vec<Result<Bytes, std::io::Error>> {
        bytes_chunks
            .into_iter()
            .map(|s| Ok(Bytes::from(s.to_string())))
            .collect()
    }

    #[test]
    fn convert_sse_stream_translates_events_and_appends_done() {
        let sse = "event: response.created\n\
                   data: {\"response\":{\"id\":\"resp_1\",\"created_at\":1}}\n\n\
                   event: response.output_text.delta\n\
                   data: {\"delta\":\"Hi\"}\n\n\
                   event: response.completed\n\
                   data: {}\n\n";
        let upstream = futures::stream::iter(block_from(vec![sse]));
        let converted = convert_sse_stream(upstream, "gpt-5.4".to_string());
        let out: Vec<Result<Bytes, std::io::Error>> = futures::executor::block_on(
            futures::StreamExt::collect::<Vec<_>>(converted),
        );
        let rendered: String = out
            .into_iter()
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect();

        assert!(rendered.contains("\"object\":\"chat.completion.chunk\""));
        assert!(rendered.contains("\"role\":\"assistant\""));
        assert!(rendered.contains("\"content\":\"Hi\""));
        assert!(rendered.contains("\"finish_reason\":\"stop\""));
        assert!(rendered.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn convert_sse_stream_handles_events_split_across_network_chunks() {
        // Simulate a network read splitting a single SSE block into two
        // arbitrary byte chunks that don't align to the "\n\n" boundary.
        let full = "event: response.output_text.delta\ndata: {\"delta\":\"Hi\"}\n\n";
        let (first, second) = full.split_at(20);
        let upstream = futures::stream::iter(block_from(vec![first, second]));
        let converted = convert_sse_stream(upstream, "m".to_string());
        let out: Vec<Result<Bytes, std::io::Error>> = futures::executor::block_on(
            futures::StreamExt::collect::<Vec<_>>(converted),
        );
        let rendered: String = out
            .into_iter()
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect();
        assert!(rendered.contains("\"content\":\"Hi\""));
        assert!(rendered.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn aggregate_sse_concatenates_output_text_deltas() {
        let sse = "event: response.output_text.delta\ndata: {\"delta\":\"Hello \"}\n\n\
                   event: response.output_text.delta\ndata: {\"delta\":\"world\"}\n\n\
                   event: response.completed\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n";
        let out = aggregate_sse(sse, "gpt-5.4");
        let text = out["choices"][0]["message"]["content"].as_str().unwrap();
        assert_eq!(text, "Hello world");
        assert_eq!(out["model"], "gpt-5.4");
    }

    #[test]
    fn sse_embedded_error_detects_usage_limit() {
        let sse = "event: response.failed\ndata: {\"error\":{\"type\":\"usage_limit_reached\"}}\n\n";
        assert!(sse_embedded_error(sse).is_some());
        let clean = "event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n";
        assert!(sse_embedded_error(clean).is_none());
    }
}
