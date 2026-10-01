use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};

use crate::core::model::WireFormat;
use crate::core::state::AppState;
use crate::proxy::body::buffer_body;
use crate::proxy::error_response::wire_error;
use crate::proxy::flow::handle_proxy;
use crate::users::Caller;

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

// `Caller` is inserted by `auth::middleware::require_bearer`; `Option` so a
// router built without that layer (tests) still works, as anonymous.
async fn chat_completions(
    State(s): State<AppState>,
    caller: Option<Extension<Caller>>,
    body: Body,
) -> Response {
    proxy_entry(s, WireFormat::OpenAi, caller_of(caller), body).await
}

async fn messages(
    State(s): State<AppState>,
    caller: Option<Extension<Caller>>,
    body: Body,
) -> Response {
    proxy_entry(s, WireFormat::Anthropic, caller_of(caller), body).await
}

fn caller_of(caller: Option<Extension<Caller>>) -> Caller {
    caller.map(|Extension(c)| c).unwrap_or_default()
}

async fn proxy_entry(s: AppState, wire: WireFormat, caller: Caller, body: Body) -> Response {
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
    handle_proxy(s, wire, pool_id, caller, bytes).await
}

async fn models(State(s): State<AppState>) -> Json<Value> {
    let snap = s.snapshot.load();
    let images_enabled = s.media.images_enabled();
    let mut data: Vec<Value> = snap
        .pools
        .iter()
        // Image pools are only callable (and so only listed) while images
        // are enabled.
        .filter(|p| p.pool.modality == crate::core::model::Modality::Chat || images_enabled)
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
