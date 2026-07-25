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

    if let Some(input) = obj.get_mut("input") {
        strip_ids(input);
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

/// Parse an SSE body into (event, data-json) pairs.
fn sse_events(sse_body: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in sse_body.split("\n\n") {
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
            continue;
        }
        if let Ok(json) = serde_json::from_str::<Value>(&data) {
            out.push((event, json));
        }
    }
    out
}

pub fn aggregate_sse(sse_body: &str) -> Value {
    let mut content = String::new();
    let mut resp_id = String::new();
    for (event, data) in sse_events(sse_body) {
        if event.ends_with("output_text.delta") {
            if let Some(d) = data["delta"].as_str() {
                content.push_str(d);
            }
        } else if event.ends_with("completed") {
            if let Some(id) = data["response"]["id"].as_str() {
                resp_id = id.to_string();
            }
        }
    }
    json!({
        "id": resp_id,
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    })
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
        assert_eq!(out["messages"][0]["role"], "developer");
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
    fn strips_item_ids() {
        let input = json!({ "messages": [], "input": [{"id": "msg_abc", "type": "message"}] });
        let out = transform_request(&input, "s");
        assert!(out["input"][0].get("id").is_none());
    }

    #[test]
    fn aggregate_sse_concatenates_output_text_deltas() {
        let sse = "event: response.output_text.delta\ndata: {\"delta\":\"Hello \"}\n\n\
                   event: response.output_text.delta\ndata: {\"delta\":\"world\"}\n\n\
                   event: response.completed\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n";
        let out = aggregate_sse(sse);
        let text = out["choices"][0]["message"]["content"].as_str().unwrap();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn sse_embedded_error_detects_usage_limit() {
        let sse = "event: response.failed\ndata: {\"error\":{\"type\":\"usage_limit_reached\"}}\n\n";
        assert!(sse_embedded_error(sse).is_some());
        let clean = "event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n";
        assert!(sse_embedded_error(clean).is_none());
    }
}
