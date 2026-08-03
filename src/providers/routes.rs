use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::error::AppError;
use crate::core::model::{Provider, ProviderKind, WireFormat};
const ANTHROPIC_VERSION: &str = "2023-06-01";
use crate::core::runtime::ProviderStatus;
use crate::core::state::{reload_snapshot, AppState};
use crate::providers::adapter::adapter_for;
use crate::providers::queries;
use crate::proxy::flow::credentials_for;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/providers", get(list).post(create))
        .route(
            "/admin/providers/:id",
            get(get_one).patch(patch).delete(delete),
        )
        .route("/admin/providers/:id/test", post(test_stub))
        .route("/admin/providers/:id/state", get(state_stub))
        .route("/admin/providers/:id/validate-model", post(validate_model))
        .route("/admin/providers/:id/list-models", get(list_models))
}

fn mask(p: &Provider) -> Value {
    let masked = p.api_key.as_ref().map(|k| {
        let tail = k
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("***{tail}")
    });
    json!({
        "id": &p.id,
        "name": &p.name,
        "wire_format": p.wire_format,
        "kind": p.kind,
        "base_url": &p.base_url,
        "api_key": masked,
        "upstream_model": &p.upstream_model,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

async fn list(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let ps = queries::list_providers(&s.db).await?;
    Ok(Json(Value::Array(ps.iter().map(mask).collect())))
}

async fn get_one(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let p = queries::get_provider(&s.db, &id).await?;
    Ok(Json(mask(&p)))
}

#[derive(Deserialize)]
struct CreateBody {
    id: String,
    name: String,
    wire_format: WireFormat,
    #[serde(default = "default_kind")]
    kind: ProviderKind,
    base_url: Option<String>,
    api_key: Option<String>,
    upstream_model: String,
}

fn default_kind() -> ProviderKind {
    ProviderKind::Passthrough
}

async fn create(
    State(s): State<AppState>,
    Json(b): Json<CreateBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    crate::core::error::validate_path_id(&b.id)?;
    let now = Utc::now();
    let p = Provider {
        id: b.id,
        name: b.name,
        wire_format: b.wire_format,
        kind: b.kind,
        base_url: b.base_url,
        api_key: b.api_key,
        upstream_model: b.upstream_model,
        created_at: now,
        updated_at: now,
    };
    queries::insert_provider(&s.db, &p).await?;
    reload_snapshot(&s).await?;
    Ok((StatusCode::CREATED, Json(mask(&p))))
}

async fn patch(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<queries::ProviderPatch>,
) -> Result<Json<Value>, AppError> {
    let p = queries::update_provider(&s.db, &id, &patch).await?;
    reload_snapshot(&s).await?;
    Ok(Json(mask(&p)))
}

async fn delete(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, AppError> {
    queries::delete_provider(&s.db, &id).await?;
    reload_snapshot(&s).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn test_stub(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    let url = match &provider.base_url {
        Some(u) => u.clone(),
        None => return Ok(Json(json!({ "ok": false, "reason": "no base_url (oauth provider)" }))),
    };
    let res = s.http.get(&url).send().await;
    match res {
        Ok(r) => Ok(Json(json!({ "ok": true, "status": r.status().as_u16() }))),
        Err(e) => Ok(Json(json!({ "ok": false, "reason": e.to_string() }))),
    }
}

#[derive(Deserialize)]
struct ValidateModelBody {
    model: Option<String>,
}

/// Sends a minimal real chat request ("hi") through the provider's own
/// adapter with a chosen model swapped in, to confirm the model name is
/// actually callable before saving it as a pool member's `model_override`.
/// Reuses `ProviderAdapter::build_request` (same code path the real proxy
/// uses) so this exercises the exact auth/request-shape logic per
/// wire_format/kind, rather than re-deriving it - no half-refresh handling
/// though: a token that merely needs refreshing will show up as a failure
/// here, which is still an accurate "this can't be used right now" signal.
async fn validate_model(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ValidateModelBody>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    let model = body
        .model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| provider.upstream_model.clone());
    let probe = Provider {
        upstream_model: model,
        ..provider
    };

    let test_body = json!({
        "model": probe.upstream_model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 8
    });
    let body_bytes = Bytes::from(serde_json::to_vec(&test_body).unwrap());

    let creds = credentials_for(&s, &probe).await;
    let adapter = adapter_for(&probe, s.http.clone());
    let req = adapter.build_request(&body_bytes, &creds).await?;

    match s.http.execute(req).await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Ok(Json(json!({ "ok": true, "status": status.as_u16() })))
            } else {
                let text = resp.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(300).collect();
                Ok(Json(json!({ "ok": false, "status": status.as_u16(), "message": snippet })))
            }
        }
        Err(e) => Ok(Json(json!({ "ok": false, "message": e.to_string() }))),
    }
}

/// A provider's `base_url` is the full chat/messages endpoint, not a base
/// path - swap its last path segment for `models` rather than requiring a
/// second URL field just for this. Falls back to appending `/models` if the
/// URL doesn't end in a recognized segment (a non-standard mirror path).
fn derive_models_url(base_url: &str) -> String {
    for suffix in ["/chat/completions", "/messages"] {
        if let Some(prefix) = base_url.strip_suffix(suffix) {
            return format!("{prefix}/models");
        }
    }
    match base_url.rfind('/') {
        Some(idx) => format!("{}/models", &base_url[..idx]),
        None => format!("{base_url}/models"),
    }
}

/// Fetches the provider's own live model list (its `GET .../models`), for
/// populating the model-override suggestions with reality instead of a
/// static array that inevitably goes stale (see the deepseek-chat ->
/// deepseek-v4-flash rename). Best-effort only: Codex OAuth has no
/// discoverable models endpoint (that's why onboarding.rs probes a candidate
/// list instead), and some passthrough mirrors won't implement `/models`
/// either - both report `ok: false` with a reason rather than erroring, so
/// the frontend can fall back to its static suggestions.
async fn list_models(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    if provider.kind != ProviderKind::Passthrough {
        return Ok(Json(json!({
            "ok": false,
            "reason": "this provider kind has no discoverable /models endpoint"
        })));
    }
    let base_url = match &provider.base_url {
        Some(u) => u,
        None => return Ok(Json(json!({ "ok": false, "reason": "provider has no base_url" }))),
    };

    let mut builder = s.http.get(derive_models_url(base_url));
    if let Some(key) = provider.api_key.as_ref() {
        builder = match provider.wire_format {
            WireFormat::OpenAi => builder.bearer_auth(key),
            WireFormat::Anthropic => builder
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION),
        };
    }

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return Ok(Json(json!({ "ok": false, "reason": e.to_string() }))),
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Ok(Json(json!({
            "ok": false,
            "reason": format!("HTTP {}: {snippet}", status.as_u16())
        })));
    }
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Ok(Json(json!({
                "ok": false,
                "reason": format!("could not parse response as JSON: {e}")
            })))
        }
    };
    let models: Vec<String> = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return Ok(Json(json!({
            "ok": false,
            "reason": "response had no recognizable model list (expected {\"data\":[{\"id\":...}]})"
        })));
    }
    Ok(Json(json!({ "ok": true, "models": models })))
}

async fn state_stub(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    queries::get_provider(&s.db, &id).await?;
    let entry = s.runtime.get(&id);
    let (level, status, until_secs) = match entry {
        Some(st) => {
            let secs = st
                .unavailable_until
                .map(|u| u.saturating_duration_since(std::time::Instant::now()).as_secs());
            let status = match st.status {
                ProviderStatus::Healthy => "healthy",
                ProviderStatus::Cooling => "cooling",
                ProviderStatus::Misconfigured => "misconfigured",
            };
            (st.backoff_level, status, secs)
        }
        None => (0u8, "healthy", None),
    };
    Ok(Json(json!({
        "provider_id": id,
        "backoff_level": level,
        "status": status,
        "unavailable_in_secs": until_secs,
    })))
}

#[cfg(test)]
mod tests {
    use super::derive_models_url;

    #[test]
    fn derive_models_url_swaps_the_known_endpoint_suffixes() {
        assert_eq!(
            derive_models_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            derive_models_url("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/models"
        );
    }

    #[test]
    fn derive_models_url_falls_back_to_swapping_the_last_segment() {
        assert_eq!(
            derive_models_url("https://mirror.example.com/api/completion"),
            "https://mirror.example.com/api/models"
        );
    }
}
