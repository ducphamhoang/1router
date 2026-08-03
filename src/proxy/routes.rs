use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::core::model::WireFormat;
use crate::core::state::AppState;
use crate::proxy::body::buffer_body;
use crate::proxy::error_response::wire_error;
use crate::proxy::flow::handle_proxy;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
}

fn model_from_body(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(bytes).ok().and_then(|v| {
        v.get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    })
}

async fn chat_completions(State(s): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    proxy_entry(s, WireFormat::OpenAi, headers, body).await
}

async fn messages(State(s): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    proxy_entry(s, WireFormat::Anthropic, headers, body).await
}

async fn proxy_entry(s: AppState, wire: WireFormat, headers: HeaderMap, body: Body) -> Response {
    let cap = s.config.max_body_bytes;
    let bytes = match buffer_body(body, cap).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let pool_id = match model_from_body(&bytes) {
        Some(m) => m,
        None => {
            return wire_error(
                wire,
                axum::http::StatusCode::BAD_REQUEST,
                "missing 'model' field",
            )
        }
    };
    handle_proxy(s, wire, pool_id, headers, bytes).await
}

async fn models(State(s): State<AppState>) -> Json<Value> {
    let snap = s.snapshot.load();
    let mut data: Vec<Value> = snap
        .pools
        .iter()
        .map(|p| json!({ "id": p.pool.id, "object": "model", "owned_by": "1router" }))
        .collect();

    // <provider_id>/<model> entries for anything a live `/models` fetch has
    // found (on provider creation, or via the admin UI's fetch actions) -
    // cheap, since it's an in-memory cache read, not a network call. No
    // dedup needed against the pool ids above: pool ids can never contain
    // '/', so the two sets can't overlap.
    for entry in s.discovered_models.iter() {
        let provider_id = entry.key();
        for model in entry.value() {
            data.push(json!({
                "id": format!("{provider_id}/{model}"),
                "object": "model",
                "owned_by": "1router"
            }));
        }
    }

    Json(json!({ "object": "list", "data": data }))
}
