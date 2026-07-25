use serde_json::{json, Value};

const DISALLOWED: &[&str] = &[
    "temperature",
    "top_p",
    "max_tokens",
    "max_output_tokens",
    "user",
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
}
