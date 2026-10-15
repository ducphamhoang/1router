//! OpenAI Responses API (`POST /v1/responses`) front door.
//!
//! Codex CLI (and other recent OpenAI clients) only speak the Responses
//! protocol. Rather than teach the failover flow a third wire format, this
//! module translates the request to a Chat Completions request, runs it
//! through the normal OpenAI-wire flow (pools, failover, logging), and
//! translates the reply - buffered JSON or SSE - back to Responses shapes.
//!
//! Supported: text + image input, `instructions`, function tools, tool
//! calls and their outputs, streaming, usage. Reasoning items and
//! non-function tools (web search, local shell, custom/freeform tools) are
//! dropped; the model can still use function tools.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::providers::adapter::sse::SseFramer;

/// Largest upstream JSON reply buffered for translation.
const MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

// ------------------------------------------------------------- namespaces

/// Flattened chat tool name -> (namespace, tool name).
pub type NamespaceMap = HashMap<String, (String, String)>;

fn flat_name(ns: &str, name: &str) -> String {
    format!("{ns}__{name}")
}

/// Codex groups some tools under `{"type":"namespace","tools":[...]}`. Chat
/// has no such thing, so each member becomes a plain function named
/// `<namespace>__<tool>`; this records how to undo that on the way back.
pub fn namespaces_of(req: &Value) -> NamespaceMap {
    let mut m = NamespaceMap::new();
    for t in req.get("tools").and_then(|t| t.as_array()).into_iter().flatten() {
        if t["type"] != "namespace" {
            continue;
        }
        let Some(ns) = t["name"].as_str() else { continue };
        for inner in t["tools"].as_array().into_iter().flatten() {
            if let (true, Some(n)) = (inner["type"] == "function", inner["name"].as_str()) {
                m.insert(flat_name(ns, n), (ns.to_string(), n.to_string()));
            }
        }
    }
    m
}

/// Apply the namespace split to a `function_call` output item.
fn with_namespace(mut item: Value, map: &NamespaceMap) -> Value {
    let hit = item["name"].as_str().and_then(|n| map.get(n)).cloned();
    if let Some((ns, name)) = hit {
        item["namespace"] = json!(ns);
        item["name"] = json!(name);
    }
    item
}

// ---------------------------------------------------------------- request

fn text_of_parts(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn chat_content(content: &Value) -> Value {
    let Value::Array(parts) = content else {
        return Value::String(text_of_parts(content));
    };
    let mut out = Vec::new();
    for p in parts {
        match p.get("type").and_then(|t| t.as_str()) {
            Some("input_image") => {
                let url = p.get("image_url").cloned().unwrap_or(Value::Null);
                // `image_url` is a string in Responses, an object in chat.
                let url = match url {
                    Value::String(s) => json!({ "url": s }),
                    other => other,
                };
                out.push(json!({ "type": "image_url", "image_url": url }));
            }
            _ => {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push(json!({ "type": "text", "text": t }));
                }
            }
        }
    }
    if out.iter().all(|p| p["type"] == "text") {
        Value::String(
            out.iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        )
    } else {
        Value::Array(out)
    }
}

/// Appends a tool call to the trailing assistant message, or opens one.
fn push_tool_call(messages: &mut Vec<Value>, call: Value) {
    if let Some(last) = messages.last_mut() {
        if last["role"] == "assistant" {
            if let Some(arr) = last.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                arr.push(call);
                return;
            }
            last["tool_calls"] = json!([call]);
            return;
        }
    }
    messages.push(json!({ "role": "assistant", "content": null, "tool_calls": [call] }));
}

/// Responses request body -> Chat Completions request body.
pub fn request_to_chat(req: &Value) -> Result<Value, String> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or("missing 'model' field")?;
    let mut messages: Vec<Value> = Vec::new();

    if let Some(ins) = req.get("instructions").and_then(|i| i.as_str()) {
        if !ins.is_empty() {
            messages.push(json!({ "role": "system", "content": ins }));
        }
    }

    match req.get("input") {
        Some(Value::String(s)) => messages.push(json!({ "role": "user", "content": s })),
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item.get("type").and_then(|t| t.as_str());
                match kind {
                    Some("function_call") => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("call_0");
                        push_tool_call(
                            &mut messages,
                            json!({
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": match (item["namespace"].as_str(), item["name"].as_str()) {
                                        (Some(ns), Some(n)) => json!(flat_name(ns, n)),
                                        _ => item.get("name").cloned().unwrap_or(Value::Null),
                                    },
                                    "arguments": item.get("arguments").cloned()
                                        .unwrap_or_else(|| Value::String("{}".into())),
                                }
                            }),
                        );
                    }
                    Some("function_call_output") => {
                        let out = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) if v.is_array() => text_of_parts(v),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                            "content": out,
                        }));
                    }
                    // `message`, or an untyped {role, content} easy message.
                    Some("message") | None => {
                        let role = match item.get("role").and_then(|r| r.as_str()) {
                            Some("developer") => "system",
                            Some(r) => r,
                            None => "user",
                        };
                        let content = item.get("content").cloned().unwrap_or(Value::Null);
                        let content = if role == "assistant" {
                            Value::String(text_of_parts(&content))
                        } else {
                            chat_content(&content)
                        };
                        messages.push(json!({ "role": role, "content": content }));
                    }
                    // reasoning, web_search_call, ... have no chat equivalent.
                    _ => {}
                }
            }
        }
        _ => return Err("missing 'input' field".into()),
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));

    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    if stream {
        out.insert("stream".into(), json!(true));
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mut flat: Vec<(Option<&str>, &Value)> = Vec::new();
        for t in tools {
            match t["type"].as_str() {
                Some("function") => flat.push((None, t)),
                Some("namespace") => {
                    let ns = t["name"].as_str();
                    for inner in t["tools"].as_array().into_iter().flatten() {
                        if inner["type"] == "function" {
                            flat.push((ns, inner));
                        }
                    }
                }
                _ => {}
            }
        }
        let tools: Vec<Value> = flat
            .into_iter()
            .map(|(ns, t)| {
                let name = match (ns, t["name"].as_str()) {
                    (Some(ns), Some(n)) => json!(flat_name(ns, n)),
                    _ => t["name"].clone(),
                };
                json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    }
                })
            })
            .collect();
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
            match req.get("tool_choice") {
                Some(Value::String(s)) => {
                    out.insert("tool_choice".into(), json!(s));
                }
                Some(v) if v["type"] == "function" => {
                    out.insert(
                        "tool_choice".into(),
                        json!({ "type": "function", "function": { "name": match (v["namespace"].as_str(), v["name"].as_str()) {
                            (Some(ns), Some(n)) => json!(flat_name(ns, n)),
                            _ => v["name"].clone(),
                        } } }),
                    );
                }
                _ => {}
            }
            if let Some(p) = req.get("parallel_tool_calls") {
                out.insert("parallel_tool_calls".into(), p.clone());
            }
        }
    }

    if let Some(v) = req.get("max_output_tokens") {
        out.insert("max_tokens".into(), v.clone());
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = req.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(k) = req.get("prompt_cache_key").filter(|k| k.is_string()) {
        out.insert("prompt_cache_key".into(), k.clone());
    }
    if let Some(e) = crate::core::reasoning::effort_from_responses(req) {
        out.insert("reasoning_effort".into(), json!(e));
    }
    Ok(Value::Object(out))
}

// --------------------------------------------------------- buffered reply

/// Chat `usage` -> Responses `usage`. `prompt_tokens_details.cached_tokens`
/// becomes `input_tokens_details.cached_tokens` (always present, 0 when the
/// upstream reported none, as the Responses API does): without it Codex CLI and
/// any Responses client read every prompt-cache hit as a miss.
fn usage_of(chat: &Value) -> Value {
    let u = &chat["usage"];
    let i = u["prompt_tokens"].as_i64().unwrap_or(0);
    let o = u["completion_tokens"].as_i64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
    json!({
        "input_tokens": i,
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens": o,
        "total_tokens": u["total_tokens"].as_i64().unwrap_or(i + o),
    })
}

/// Chat Completions JSON reply -> Responses object.
pub fn chat_to_response(chat: &Value, model: &str, ns: &NamespaceMap) -> Value {
    let choice = &chat["choices"][0];
    let msg = &choice["message"];
    let mut output = Vec::new();

    let text = msg["content"].as_str().unwrap_or("");
    if !text.is_empty() {
        output.push(json!({
            "type": "message", "id": new_id("msg"), "status": "completed", "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }));
    }
    if let Some(calls) = msg["tool_calls"].as_array() {
        for c in calls {
            let id = c["id"].as_str().map(str::to_string).unwrap_or_else(|| new_id("call"));
            output.push(with_namespace(json!({
                "type": "function_call", "id": new_id("fc"), "status": "completed",
                "call_id": id,
                "name": c["function"]["name"],
                "arguments": c["function"]["arguments"].as_str().unwrap_or("{}"),
            }), ns));
        }
    }

    let truncated = choice["finish_reason"] == "length";
    json!({
        "id": new_id("resp"),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": if truncated { "incomplete" } else { "completed" },
        "incomplete_details": if truncated { json!({"reason": "max_output_tokens"}) } else { Value::Null },
        "error": null,
        "model": chat["model"].as_str().unwrap_or(model),
        "output": output,
        "usage": usage_of(chat),
    })
}

// ----------------------------------------------------------------- stream

struct OpenItem {
    output_index: usize,
    item_id: String,
    /// `None` for the text message, `Some(chat tool_call index)` for calls.
    tool_index: Option<i64>,
    call_id: String,
    name: String,
    buf: String,
}

/// Chat chunk stream -> Responses event stream (as a state machine).
pub struct StreamState {
    seq: u64,
    resp_id: String,
    model: String,
    created_at: i64,
    started: bool,
    next_index: usize,
    text: Option<OpenItem>,
    calls: Vec<OpenItem>,
    finished_items: Vec<Value>,
    usage: Value,
    truncated: bool,
    finished: bool,
    ns: NamespaceMap,
}

fn sse(ev: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {ev}\ndata: {data}\n\n"))
}

impl StreamState {
    pub fn new(model: &str, ns: NamespaceMap) -> Self {
        StreamState {
            seq: 0,
            resp_id: new_id("resp"),
            model: model.to_string(),
            created_at: chrono::Utc::now().timestamp(),
            started: false,
            next_index: 0,
            text: None,
            calls: Vec::new(),
            finished_items: Vec::new(),
            usage: json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
            truncated: false,
            finished: false,
            ns,
        }
    }

    fn emit(&mut self, out: &mut Vec<Bytes>, ev: &str, mut data: Value) {
        data["type"] = json!(ev);
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        out.push(sse(ev, &data));
    }

    fn response_obj(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.resp_id, "object": "response", "created_at": self.created_at,
            "status": status,
            "incomplete_details": if status == "incomplete" { json!({"reason": "max_output_tokens"}) } else { Value::Null },
            "error": null, "model": self.model, "output": output, "usage": self.usage,
        })
    }

    fn start(&mut self, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let r = self.response_obj("in_progress", vec![]);
        self.emit(out, "response.created", json!({ "response": r.clone() }));
        self.emit(out, "response.in_progress", json!({ "response": r }));
    }

    fn close_text(&mut self, out: &mut Vec<Bytes>) {
        let Some(t) = self.text.take() else { return };
        let (id, idx, full) = (t.item_id.clone(), t.output_index, t.buf.clone());
        self.emit(out, "response.output_text.done", json!({
            "item_id": id, "output_index": idx, "content_index": 0, "text": full }));
        let part = json!({ "type": "output_text", "text": full, "annotations": [] });
        self.emit(out, "response.content_part.done", json!({
            "item_id": id, "output_index": idx, "content_index": 0, "part": part }));
        let item = json!({ "type": "message", "id": id, "status": "completed", "role": "assistant",
            "content": [part] });
        self.emit(out, "response.output_item.done", json!({ "output_index": idx, "item": item.clone() }));
        self.finished_items.push(item);
    }

    fn close_call(&mut self, out: &mut Vec<Bytes>, c: OpenItem) {
        self.emit(out, "response.function_call_arguments.done", json!({
            "item_id": c.item_id, "output_index": c.output_index, "arguments": c.buf }));
        let item = with_namespace(json!({ "type": "function_call", "id": c.item_id, "status": "completed",
            "call_id": c.call_id, "name": c.name, "arguments": c.buf }), &self.ns);
        self.emit(out, "response.output_item.done", json!({ "output_index": c.output_index, "item": item.clone() }));
        self.finished_items.push(item);
    }

    /// Feed one parsed chat-completion chunk.
    pub fn feed(&mut self, chunk: &Value, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.start(out);
        if let Some(m) = chunk["model"].as_str() {
            self.model = m.to_string();
        }
        if chunk["usage"].is_object() {
            self.usage = usage_of(chunk);
        }
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];

        if let Some(txt) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            if self.text.is_none() {
                let (idx, id) = (self.next_index, new_id("msg"));
                self.next_index += 1;
                self.emit(out, "response.output_item.added", json!({
                    "output_index": idx,
                    "item": { "type": "message", "id": id, "status": "in_progress",
                              "role": "assistant", "content": [] } }));
                self.emit(out, "response.content_part.added", json!({
                    "item_id": id, "output_index": idx, "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] } }));
                self.text = Some(OpenItem {
                    output_index: idx, item_id: id, tool_index: None,
                    call_id: String::new(), name: String::new(), buf: String::new(),
                });
            }
            let t = self.text.as_mut().unwrap();
            t.buf.push_str(txt);
            let (id, idx) = (t.item_id.clone(), t.output_index);
            self.emit(out, "response.output_text.delta", json!({
                "item_id": id, "output_index": idx, "content_index": 0, "delta": txt }));
        }

        if let Some(calls) = delta["tool_calls"].as_array() {
            // A tool call ends the text message.
            self.close_text(out);
            for c in calls {
                let ti = c["index"].as_i64().unwrap_or(0);
                let pos = match self.calls.iter().position(|o| o.tool_index == Some(ti)) {
                    Some(p) => p,
                    None => {
                        let (idx, id) = (self.next_index, new_id("fc"));
                        self.next_index += 1;
                        let call_id = c["id"].as_str().map(str::to_string).unwrap_or_else(|| new_id("call"));
                        let name = c["function"]["name"].as_str().unwrap_or("").to_string();
                        self.emit(out, "response.output_item.added", json!({
                            "output_index": idx,
                            "item": with_namespace(json!({ "type": "function_call", "id": id, "status": "in_progress",
                                      "call_id": call_id, "name": name, "arguments": "" }), &self.ns) }));
                        self.calls.push(OpenItem {
                            output_index: idx, item_id: id, tool_index: Some(ti),
                            call_id, name, buf: String::new(),
                        });
                        self.calls.len() - 1
                    }
                };
                if let Some(a) = c["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
                    let o = &mut self.calls[pos];
                    o.buf.push_str(a);
                    let (id, idx) = (o.item_id.clone(), o.output_index);
                    self.emit(out, "response.function_call_arguments.delta", json!({
                        "item_id": id, "output_index": idx, "delta": a }));
                }
            }
        }

        if choice["finish_reason"] == "length" {
            self.truncated = true;
        }
    }

    /// Upstream ended: close open items and send `response.completed`.
    pub fn finish(&mut self, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.start(out);
        self.close_text(out);
        for c in std::mem::take(&mut self.calls) {
            self.close_call(out, c);
        }
        self.finished = true;
        let items = std::mem::take(&mut self.finished_items);
        let (status, ev) = if self.truncated {
            ("incomplete", "response.incomplete")
        } else {
            ("completed", "response.completed")
        };
        let r = self.response_obj(status, items);
        self.emit(out, ev, json!({ "response": r }));
    }
}

fn feed_block(st: &mut StreamState, block: &str, out: &mut Vec<Bytes>) {
    for line in block.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            st.feed(&v, out);
        }
    }
}

fn translate_stream(upstream: Body, model: String, ns: NamespaceMap) -> Body {
    struct Tx {
        up: axum::body::BodyDataStream,
        framer: SseFramer,
        st: StreamState,
        queue: VecDeque<Bytes>,
        done: bool,
    }
    let tx = Tx {
        up: upstream.into_data_stream(),
        framer: SseFramer::default(),
        st: StreamState::new(&model, ns),
        queue: VecDeque::new(),
        done: false,
    };
    let s = futures::stream::unfold(tx, |mut tx| async move {
        loop {
            if let Some(b) = tx.queue.pop_front() {
                return Some((Ok::<Bytes, Infallible>(b), tx));
            }
            if tx.done {
                return None;
            }
            let mut out = Vec::new();
            match tx.up.next().await {
                Some(Ok(chunk)) => {
                    let ok = tx.framer.push(&chunk);
                    while let Some(block) = tx.framer.next_block() {
                        feed_block(&mut tx.st, &block, &mut out);
                    }
                    if !ok {
                        tx.done = true;
                        tx.st.finish(&mut out);
                    }
                }
                _ => {
                    let rest = tx.framer.finish();
                    feed_block(&mut tx.st, &rest, &mut out);
                    tx.st.finish(&mut out);
                    tx.done = true;
                }
            }
            tx.queue.extend(out);
        }
    });
    Body::from_stream(s)
}

// ------------------------------------------------------------------ entry

fn responses_error(status: StatusCode, msg: &str) -> Response {
    crate::proxy::error_response::wire_error(crate::core::model::WireFormat::OpenAi, status, msg)
}

/// Translate an OpenAI-wire reply from the proxy flow into Responses form.
async fn translate_reply(
    resp: Response,
    model: &str,
    wanted_stream: bool,
    ns: NamespaceMap,
) -> Response {
    if !resp.status().is_success() {
        // The chat error envelope is already what Responses clients expect.
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let ctype = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // A streaming request answered without a JSON content type is SSE even
    // when the upstream forgot to label it.
    let is_sse = ctype.contains("text/event-stream") || (wanted_stream && !ctype.contains("json"));
    parts.headers.remove(header::CONTENT_LENGTH);

    if is_sse {
        return Response::from_parts(parts, translate_stream(body, model.to_string(), ns));
    }
    let bytes = match crate::proxy::body::buffer_body(body, MAX_REPLY_BYTES).await {
        Ok(b) => b,
        Err(_) => return responses_error(StatusCode::BAD_GATEWAY, "upstream reply too large"),
    };
    let Ok(chat) = serde_json::from_slice::<Value>(&bytes) else {
        return responses_error(StatusCode::BAD_GATEWAY, "upstream returned non-JSON reply");
    };
    let r = chat_to_response(&chat, model, &ns);
    if wanted_stream {
        // Upstream answered a streaming request with plain JSON; replay it
        // as a one-shot event sequence so the client still gets SSE.
        let mut st = StreamState::new(model, ns);
        let mut out = Vec::new();
        let choice = &chat["choices"][0];
        st.feed(&json!({
            "model": chat["model"], "usage": chat["usage"],
            "choices": [{ "delta": {
                "content": choice["message"]["content"],
                "tool_calls": choice["message"]["tool_calls"].as_array().map(|a| {
                    a.iter().enumerate().map(|(i, c)| json!({
                        "index": i, "id": c["id"], "function": c["function"] })).collect::<Vec<_>>()
                }),
            }, "finish_reason": choice["finish_reason"] }],
        }), &mut out);
        st.finish(&mut out);
        parts.headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        let body = Body::from(out.concat());
        return Response::from_parts(parts, body);
    }
    parts.headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Response::from_parts(parts, Body::from(r.to_string()))
}

pub async fn entry(
    s: crate::core::state::AppState,
    caller: crate::users::Caller,
    body: Body,
) -> Response {
    let bytes = match crate::proxy::body::buffer_body(body, s.config.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let req: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return responses_error(StatusCode::BAD_REQUEST, "request body is not valid JSON"),
    };
    let chat = match request_to_chat(&req) {
        Ok(c) => c,
        Err(m) => return responses_error(StatusCode::BAD_REQUEST, &m),
    };
    let ns = namespaces_of(&req);
    let model = chat["model"].as_str().unwrap_or_default().to_string();
    let wanted_stream = chat.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let resp = crate::proxy::flow::handle_proxy(
        s,
        crate::core::model::WireFormat::OpenAi,
        model.clone(),
        caller,
        Bytes::from(chat.to_string()),
    )
    .await;
    translate_reply(resp, &model, wanted_stream, ns).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_instructions_input_items_and_tools() {
        let req = json!({
            "model": "m", "instructions": "be brief", "stream": true,
            "max_output_tokens": 50,
            "tools": [
                {"type": "function", "name": "sh", "description": "run", "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "tool_choice": {"type": "function", "name": "sh"},
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev"}]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hi"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA"}]},
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "c1", "name": "sh", "arguments": "{\"a\":1}"},
                {"type": "function_call", "call_id": "c2", "name": "sh", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "done"}
            ]
        });
        let c = request_to_chat(&req).unwrap();
        assert_eq!(c["stream"], true);
        assert_eq!(c["stream_options"]["include_usage"], true);
        assert_eq!(c["max_tokens"], 50);
        assert_eq!(c["tools"].as_array().unwrap().len(), 1);
        assert_eq!(c["tools"][0]["function"]["name"], "sh");
        assert_eq!(c["tool_choice"]["function"]["name"], "sh");
        let m = c["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "be brief"}));
        assert_eq!(m[1], json!({"role": "system", "content": "dev"}));
        assert_eq!(m[2]["content"][0]["type"], "text");
        assert_eq!(m[2]["content"][1]["image_url"]["url"], "data:image/png;base64,AA");
        // assistant text then two calls merge into one assistant message
        assert_eq!(m[3]["role"], "assistant");
        assert_eq!(m[3]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(m[3]["tool_calls"][1]["id"], "c2");
        assert_eq!(m[4], json!({"role": "tool", "tool_call_id": "c1", "content": "done"}));
        assert_eq!(m.len(), 5);
    }

    #[test]
    fn reasoning_effort_is_forwarded_as_chat_reasoning_effort() {
        let req = json!({"model": "m", "input": "hi", "reasoning": {"effort": "high", "summary": "auto"}});
        assert_eq!(request_to_chat(&req).unwrap()["reasoning_effort"], "high");
        let req = json!({"model": "m", "input": "hi"});
        assert!(request_to_chat(&req).unwrap().get("reasoning_effort").is_none());
    }

    #[test]
    fn namespace_tools_are_flattened_and_restored() {
        let req = json!({
            "model": "m", "input": [
                {"type": "function_call", "call_id": "c", "namespace": "agents", "name": "spawn", "arguments": "{}"}],
            "tools": [{"type": "namespace", "name": "agents", "tools": [
                {"type": "function", "name": "spawn", "parameters": {"type": "object"}}]}]
        });
        let c = request_to_chat(&req).unwrap();
        assert_eq!(c["tools"][0]["function"]["name"], "agents__spawn");
        assert_eq!(c["messages"][0]["tool_calls"][0]["function"]["name"], "agents__spawn");
        let ns = namespaces_of(&req);
        let chat = json!({"choices": [{"message": {"tool_calls": [
            {"id": "t", "function": {"name": "agents__spawn", "arguments": "{}"}}]}}]});
        let r = chat_to_response(&chat, "m", &ns);
        assert_eq!(r["output"][0]["name"], "spawn");
        assert_eq!(r["output"][0]["namespace"], "agents");
        let mut st = StreamState::new("m", ns);
        let mut out = Vec::new();
        st.feed(&json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "t", "function": {"name": "agents__spawn", "arguments": "{}"}}]}}]}), &mut out);
        st.finish(&mut out);
        let last = events(&out).pop().unwrap().1;
        let done = &last["response"]["output"][0];
        assert_eq!(done["namespace"], "agents");
        assert_eq!(done["name"], "spawn");
    }

    #[test]
    fn string_input_and_missing_input() {
        let c = request_to_chat(&json!({"model": "m", "input": "yo"})).unwrap();
        assert_eq!(c["messages"][0], json!({"role": "user", "content": "yo"}));
        assert!(c.get("stream").is_none());
        assert!(request_to_chat(&json!({"model": "m"})).is_err());
        assert!(request_to_chat(&json!({"input": "x"})).is_err());
    }

    #[test]
    fn chat_reply_becomes_response_with_text_and_calls() {
        let chat = json!({
            "model": "up", "usage": {"prompt_tokens": 3, "completion_tokens": 4},
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": "hello",
                "tool_calls": [{"id": "t1", "function": {"name": "sh", "arguments": "{\"x\":1}"}}]}}]
        });
        let r = chat_to_response(&chat, "m", &NamespaceMap::new());
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["model"], "up");
        assert_eq!(r["output"][0]["content"][0]["text"], "hello");
        assert_eq!(r["output"][1]["type"], "function_call");
        assert_eq!(r["output"][1]["call_id"], "t1");
        assert_eq!(r["usage"]["total_tokens"], 7);
        assert_eq!(r["usage"]["input_tokens_details"]["cached_tokens"], 0);
    }

    #[test]
    fn cached_prompt_tokens_reach_the_responses_usage() {
        let chat = json!({
            "model": "up",
            "usage": {"prompt_tokens": 1000, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 896}},
            "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]
        });
        let r = chat_to_response(&chat, "m", &NamespaceMap::new());
        assert_eq!(r["usage"]["input_tokens"], 1000);
        assert_eq!(r["usage"]["input_tokens_details"]["cached_tokens"], 896);
    }

    #[test]
    fn truncated_reply_is_incomplete() {
        let chat = json!({"choices": [{"finish_reason": "length", "message": {"content": "x"}}]});
        assert_eq!(chat_to_response(&chat, "m", &NamespaceMap::new())["status"], "incomplete");
    }

    fn events(out: &[Bytes]) -> Vec<(String, Value)> {
        out.iter()
            .map(|b| {
                let s = std::str::from_utf8(b).unwrap();
                let ev = s.lines().next().unwrap().trim_start_matches("event: ").to_string();
                let data = s.lines().nth(1).unwrap().trim_start_matches("data: ");
                (ev, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    #[test]
    fn stream_text_then_tool_call_emits_ordered_events() {
        let mut st = StreamState::new("m", NamespaceMap::new());
        let mut out = Vec::new();
        st.feed(&json!({"choices": [{"delta": {"content": "He"}}]}), &mut out);
        st.feed(&json!({"choices": [{"delta": {"content": "llo"}}]}), &mut out);
        st.feed(&json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "t1", "function": {"name": "sh", "arguments": "{\"a\""}}]}}]}), &mut out);
        st.feed(&json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": ":1}"}}]}, "finish_reason": "tool_calls"}]}), &mut out);
        st.feed(&json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}}), &mut out);
        st.finish(&mut out);
        let ev = events(&out);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names[0], "response.created");
        assert!(names.contains(&"response.output_text.delta"));
        assert!(names.contains(&"response.function_call_arguments.delta"));
        assert_eq!(*names.last().unwrap(), "response.completed");
        // sequence numbers are strictly increasing from 0
        for (i, (_, d)) in ev.iter().enumerate() {
            assert_eq!(d["sequence_number"], i);
        }
        let done = &ev.last().unwrap().1["response"];
        assert_eq!(done["usage"]["total_tokens"], 3);
        let output = done["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["content"][0]["text"], "Hello");
        assert_eq!(output[1]["arguments"], "{\"a\":1}");
        assert_eq!(output[1]["call_id"], "t1");
    }

    #[test]
    fn finish_without_any_chunk_still_completes() {
        let mut st = StreamState::new("m", NamespaceMap::new());
        let mut out = Vec::new();
        st.finish(&mut out);
        let ev = events(&out);
        assert_eq!(ev.first().unwrap().0, "response.created");
        assert_eq!(ev.last().unwrap().0, "response.completed");
    }
}
