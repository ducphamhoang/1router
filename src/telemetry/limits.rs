//! Rate-limit visibility for `/v1` callers: `GET /v1/limits`.
//!
//! Upstreams report how much of an account's quota is left in response
//! headers (ChatGPT/Codex: `x-codex-primary-used-percent`,
//! `x-codex-primary-window-minutes`, `x-codex-primary-reset-at`, the same
//! for `secondary` and `credits`; OpenAI/Anthropic-style APIs:
//! `x-ratelimit-*` / `anthropic-ratelimit-*`). The Codex adapter never
//! relays upstream headers, so a client could only learn an account was
//! exhausted from a 503 after the fact. `proxy::flow` now hands every
//! upstream response's headers to [`capture`], which keeps the latest
//! limit headers per provider in memory (process-local, lost on restart,
//! bounded by the number of providers).
//!
//! The report combines that with each provider's runtime state
//! (healthy / cooling / misconfigured and how long until it's retried).
//! Provider ids are only shown for providers already listed by
//! `GET /v1/models` (direct `<provider_id>/<model>` addressing); pool
//! members are reported by position only, so the routing topology stays
//! as hidden as it is for the debug headers (SEC-11).

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde_json::{json, Value};

use crate::core::model::Modality;
use crate::core::runtime::{worst_provider_state, RuntimeStateMap};
use crate::core::state::AppState;

/// Most limit headers kept per provider, and the longest value kept - a
/// misbehaving upstream can't grow the store or the report.
const MAX_HEADERS: usize = 32;
const MAX_VALUE_LEN: usize = 64;

#[derive(Clone, Debug)]
pub struct LimitSnapshot {
    pub observed_at: DateTime<Utc>,
    pub values: BTreeMap<String, String>,
}

fn store() -> &'static DashMap<String, LimitSnapshot> {
    static STORE: OnceLock<DashMap<String, LimitSnapshot>> = OnceLock::new();
    STORE.get_or_init(DashMap::new)
}

/// Header names (lowercase, as `HeaderName` stores them) that describe quota.
pub fn is_limit_header(name: &str) -> bool {
    name.starts_with("x-codex-primary-")
        || name.starts_with("x-codex-secondary-")
        || name.starts_with("x-codex-credits-")
        || name.starts_with("x-ratelimit-")
        || name.starts_with("anthropic-ratelimit-")
}

/// The limit headers of one upstream response, capped in count and length.
pub fn extract(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers.iter() {
        let name = name.as_str();
        if !is_limit_header(name) {
            continue;
        }
        let Ok(text) = value.to_str() else { continue };
        if text.len() > MAX_VALUE_LEN {
            continue;
        }
        if out.len() >= MAX_HEADERS {
            break;
        }
        out.insert(name.to_string(), text.to_string());
    }
    out
}

/// Remember `provider_id`'s latest limit headers. Responses without any
/// (most error pages) keep the previous snapshot.
pub fn capture(provider_id: &str, headers: &HeaderMap) {
    let values = extract(headers);
    if values.is_empty() {
        return;
    }
    store().insert(
        provider_id.to_string(),
        LimitSnapshot { observed_at: Utc::now(), values },
    );
}

pub fn snapshot(provider_id: &str) -> Option<LimitSnapshot> {
    store().get(provider_id).map(|r| r.value().clone())
}

/// One provider's entry: runtime state + last seen limit headers.
pub fn provider_report(runtime: &RuntimeStateMap, provider_id: &str, now: Instant) -> Value {
    let (available, status, retry_in_seconds) = match worst_provider_state(runtime, provider_id) {
        Some(st) => {
            let retry = st
                .unavailable_until
                .and_then(|until| until.checked_duration_since(now))
                .map(|d| d.as_secs());
            (
                st.is_available(now),
                serde_json::to_value(st.status).unwrap_or(Value::Null),
                retry,
            )
        }
        None => (true, json!("healthy"), None),
    };
    let snap = snapshot(provider_id);
    json!({
        "available": available,
        "status": status,
        "retry_in_seconds": retry_in_seconds,
        "rate_limits": snap.as_ref().map(|s| json!(s.values)).unwrap_or(Value::Null),
        "observed_at": snap.as_ref().map(|s| s.observed_at.to_rfc3339()),
    })
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/limits", get(limits))
}

async fn limits(State(s): State<AppState>) -> Json<Value> {
    let snap = s.snapshot.load();
    let now = Instant::now();

    let mut providers = serde_json::Map::new();
    for entry in s.discovered_models.iter() {
        providers.insert(entry.key().clone(), provider_report(&s.runtime, entry.key(), now));
    }

    let pools: Vec<Value> = snap
        .pools
        .iter()
        .filter(|p| p.pool.modality == Modality::Chat)
        .map(|p| {
            let members: Vec<Value> = p
                .members
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut v = provider_report(&s.runtime, &m.provider_id, now);
                    v["member"] = json!(i + 1);
                    v
                })
                .collect();
            json!({ "id": p.pool.id, "members": members })
        })
        .collect();

    Json(json!({ "object": "limits", "providers": providers, "pools": pools }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runtime::{runtime_key, ProviderRuntimeState};
    use std::sync::Arc;
    use std::time::Duration;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn extract_keeps_only_limit_headers_and_caps_values() {
        let long = "9".repeat(MAX_VALUE_LEN + 1);
        let h = headers(&[
            ("x-codex-primary-used-percent", "42"),
            ("x-codex-secondary-reset-at", "1760500000"),
            ("x-codex-credits-has-credits", "false"),
            ("x-ratelimit-remaining-tokens", "1000"),
            ("anthropic-ratelimit-requests-limit", "50"),
            ("x-codex-primary-window-minutes", long.as_str()),
            ("set-cookie", "a=b"),
            ("chatgpt-account-id", "acct_secret"),
            ("x-codex-turn-state", "opaque"),
        ]);
        let got = extract(&h);
        assert_eq!(got.get("x-codex-primary-used-percent").map(String::as_str), Some("42"));
        assert!(got.contains_key("x-codex-secondary-reset-at"));
        assert!(got.contains_key("x-codex-credits-has-credits"));
        assert!(got.contains_key("x-ratelimit-remaining-tokens"));
        assert!(got.contains_key("anthropic-ratelimit-requests-limit"));
        assert!(!got.contains_key("x-codex-primary-window-minutes"), "over-long value dropped");
        assert!(!got.contains_key("set-cookie"));
        assert!(!got.contains_key("chatgpt-account-id"));
        assert!(!got.contains_key("x-codex-turn-state"));
    }

    #[test]
    fn capture_keeps_previous_snapshot_when_response_has_no_limits() {
        let id = "limits-test-capture";
        capture(id, &headers(&[("x-codex-primary-used-percent", "10")]));
        capture(id, &headers(&[("content-type", "application/json")]));
        let snap = snapshot(id).expect("snapshot kept");
        assert_eq!(snap.values.get("x-codex-primary-used-percent").map(String::as_str), Some("10"));
        capture(id, &headers(&[("x-codex-primary-used-percent", "99")]));
        assert_eq!(
            snapshot(id).unwrap().values.get("x-codex-primary-used-percent").map(String::as_str),
            Some("99")
        );
    }

    #[test]
    fn provider_report_shows_cooling_with_retry_and_limits() {
        let runtime: RuntimeStateMap = Arc::new(DashMap::new());
        let id = "limits-test-report";
        let now = Instant::now();
        let mut st = ProviderRuntimeState::default();
        st.record_retryable(Duration::from_secs(120), now);
        runtime.insert(runtime_key(id, "gpt-6-astra"), st);
        capture(id, &headers(&[("x-codex-primary-used-percent", "100")]));

        let v = provider_report(&runtime, id, now);
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["status"], json!("cooling"));
        assert_eq!(v["retry_in_seconds"], json!(120));
        assert_eq!(v["rate_limits"]["x-codex-primary-used-percent"], json!("100"));
        assert!(v["observed_at"].is_string());
    }

    #[test]
    fn provider_report_for_unknown_provider_is_healthy_without_limits() {
        let runtime: RuntimeStateMap = Arc::new(DashMap::new());
        let v = provider_report(&runtime, "limits-test-never-seen", Instant::now());
        assert_eq!(v["available"], json!(true));
        assert_eq!(v["status"], json!("healthy"));
        assert!(v["retry_in_seconds"].is_null());
        assert!(v["rate_limits"].is_null());
    }
}
