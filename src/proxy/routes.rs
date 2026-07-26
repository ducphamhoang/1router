use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
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
    let permit = match s.proxy_semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return wire_error(
                wire,
                StatusCode::TOO_MANY_REQUESTS,
                "too many concurrent requests",
            );
        }
    };
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
    let resp = handle_proxy(s, wire, pool_id, headers, bytes).await;
    drop(permit);
    resp
}

async fn models(State(s): State<AppState>) -> Json<Value> {
    let snap = s.snapshot.load();
    let data: Vec<Value> = snap
        .pools
        .iter()
        .map(|p| json!({ "id": p.pool.id, "object": "model", "owned_by": "1router" }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::http_client::build_client;
    use crate::core::state::ConfigSnapshot;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use std::time::Duration;

    async fn state_with_proxy_permits(permits: usize) -> AppState {
        let cfg = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "s".into(),
            shared_secrets: vec!["s".into()],
            admin_secret: None,
            seed_path: None,
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            max_concurrent_requests: permits.max(1),
            allow_insecure_upstreams: true,
            drain_timeout: Duration::from_secs(1),
        };
        let db = init_pool(":memory:").await.unwrap();
        AppState {
            db,
            http: build_client(&cfg),
            config: Arc::new(cfg),
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: vec![],
                pools: vec![],
            })),
            runtime: Arc::new(dashmap::DashMap::new()),
            log_tx: tokio::sync::mpsc::channel(1).0,
            refresh_locks: Arc::new(dashmap::DashMap::new()),
            proxy_semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
        }
    }

    #[tokio::test]
    async fn proxy_entry_returns_429_when_concurrency_limit_is_exhausted() {
        let state = state_with_proxy_permits(0).await;
        let resp = proxy_entry(
            state,
            WireFormat::OpenAi,
            HeaderMap::new(),
            Body::from(r#"{"model":"gpt-4o","messages":[]}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
