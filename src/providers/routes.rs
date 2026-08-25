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
use crate::providers::adapter::{adapter_for, Credentials};
use crate::providers::adapter::commandcode;
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
        .route(
            "/admin/providers/validate-model-preview",
            post(validate_model_preview),
        )
        .route(
            "/admin/providers/list-models-preview",
            post(list_models_preview),
        )
        .route("/admin/providers/:id/list-models", get(list_models))
}

/// For OAuth-kind providers `p.api_key` is always empty - the real
/// credential lives in `provider_oauth_state.access_token` - so
/// `credential_configured` is what the admin UI checks to know a key/login
/// is already on file, distinct from whatever's masked below.
fn mask(p: &Provider, credential_configured: bool) -> Value {
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
        "credential_configured": credential_configured,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

fn is_oauth_kind(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::OauthCodex | ProviderKind::OauthCommandCode)
}

async fn list(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let ps = queries::list_providers(&s.db).await?;
    let configured_ids = queries::oauth_configured_provider_ids(&s.db).await?;
    Ok(Json(Value::Array(
        ps.iter()
            .map(|p| {
                let credential_configured = if is_oauth_kind(p.kind) {
                    configured_ids.contains(&p.id)
                } else {
                    p.api_key.is_some()
                };
                mask(p, credential_configured)
            })
            .collect(),
    )))
}

async fn get_one(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let p = queries::get_provider(&s.db, &id).await?;
    let credential_configured = if is_oauth_kind(p.kind) {
        queries::oauth_credential_configured(&s.db, &id).await?
    } else {
        p.api_key.is_some()
    };
    Ok(Json(mask(&p, credential_configured)))
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

    // Best-effort and non-blocking: the create response shouldn't wait on
    // (or fail because of) a slow/dead upstream `/models` call. Populates
    // GET /v1/models' <provider_id>/<model> listings without the caller
    // having to separately hit list-models afterward.
    spawn_bounded_discovery(s.clone(), p.clone());

    // A brand-new provider never has a credential on file yet - OAuth-kind
    // ones need a follow-up Connect/browser-login/paste step, passthrough
    // ones already reflect `p.api_key` directly.
    let credential_configured = !is_oauth_kind(p.kind) && p.api_key.is_some();
    Ok((StatusCode::CREATED, Json(mask(&p, credential_configured))))
}

async fn patch(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<queries::ProviderPatch>,
) -> Result<Json<Value>, AppError> {
    let p = queries::update_provider(&s.db, &id, &patch).await?;
    reload_snapshot(&s).await?;
    // An edit to the provider (key, base_url, model, ...) means its previous
    // runtime flags no longer describe the current config - clear them.
    // A provider can back several models (each its own runtime_key), so
    // reset every entry belonging to it, not just one lookup by bare id.
    crate::core::runtime::reset_provider_to_healthy(&s.runtime, &id);
    let credential_configured = if is_oauth_kind(p.kind) {
        queries::oauth_credential_configured(&s.db, &id).await?
    } else {
        p.api_key.is_some()
    };
    Ok(Json(mask(&p, credential_configured)))
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
        None => {
            return Ok(Json(
                json!({ "ok": false, "reason": "no base_url (oauth provider)" }),
            ))
        }
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
                // A successful probe proves this provider's credentials and
                // request shape work right now - clear any stale
                // Misconfigured/Cooling runtime flag so the proxy path stops
                // skipping it without requiring a restart. A provider can
                // back several models (each its own runtime_key), so reset
                // every entry belonging to it.
                crate::core::runtime::reset_provider_to_healthy(&s.runtime, &id);
                Ok(Json(json!({ "ok": true, "status": status.as_u16() })))
            } else {
                let text = resp.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(300).collect();
                Ok(Json(
                    json!({ "ok": false, "status": status.as_u16(), "message": snippet }),
                ))
            }
        }
        Err(e) => Ok(Json(json!({ "ok": false, "message": e.to_string() }))),
    }
}

#[derive(Deserialize)]
struct ValidateModelPreviewBody {
    wire_format: WireFormat,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

/// Same probe as `validate_model`, but for a provider that hasn't been saved
/// yet - the "Validate" button on the create form. Takes the connection
/// details straight from the form instead of loading a row from the DB, so
/// it's passthrough-only (OAuth kinds have no key/token to hand over before
/// the provider exists and its own auth dance has run).
async fn validate_model_preview(
    State(s): State<AppState>,
    Json(body): Json<ValidateModelPreviewBody>,
) -> Result<Json<Value>, AppError> {
    let now = Utc::now();
    let probe = Provider {
        id: String::new(),
        name: String::new(),
        wire_format: body.wire_format,
        kind: ProviderKind::Passthrough,
        base_url: Some(body.base_url),
        api_key: body.api_key,
        upstream_model: body.model,
        created_at: now,
        updated_at: now,
    };

    let test_body = json!({
        "model": probe.upstream_model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 8
    });
    let body_bytes = Bytes::from(serde_json::to_vec(&test_body).unwrap());

    let creds = Credentials {
        api_key: probe.api_key.clone(),
        ..Default::default()
    };
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
                Ok(Json(
                    json!({ "ok": false, "status": status.as_u16(), "message": snippet }),
                ))
            }
        }
        Err(e) => Ok(Json(json!({ "ok": false, "message": e.to_string() }))),
    }
}

/// A provider's `base_url` is the full chat/messages endpoint, not a base
/// path - swap its last path segment for `models` rather than requiring a
/// second URL field just for this. Falls back to proper URL parsing (rather
/// than a raw `rfind('/')`) for a non-standard mirror path, since blind
/// string surgery mismatches the scheme separator (`http://`) for a
/// base_url with no path at all - `rfind('/')` on `http://host:1` finds the
/// slash *inside* `//`, producing the nonsense host `http://models`.
fn derive_models_url(base_url: &str) -> String {
    for suffix in ["/chat/completions", "/messages"] {
        if let Some(prefix) = base_url.strip_suffix(suffix) {
            return format!("{prefix}/models");
        }
    }
    if let Ok(mut url) = reqwest::Url::parse(base_url) {
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty();
            segments.pop();
            segments.push("models");
        }
        url.set_query(None);
        return url.to_string();
    }
    format!("{base_url}/models")
}

/// Calls the provider's own live `GET .../models` and parses out model ids.
/// Pure network+parse - no ProviderKind/base_url checks (those produce
/// user-facing messages specific to the admin endpoint below) and no
/// caching (callers decide whether/where to cache).
pub(crate) async fn fetch_live_models(
    http: &reqwest::Client,
    provider: &Provider,
) -> Result<Vec<String>, String> {
    let base_url = provider
        .base_url
        .as_ref()
        .ok_or_else(|| "provider has no base_url".to_string())?;

    let mut builder = http.get(derive_models_url(base_url));
    if let Some(key) = provider.api_key.as_ref() {
        builder = match provider.wire_format {
            WireFormat::OpenAi => builder.bearer_auth(key),
            WireFormat::Anthropic => builder
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION),
        };
    }

    let resp = builder.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("HTTP {}: {snippet}", status.as_u16()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("could not parse response as JSON: {e}"))?;
    let models = parse_models_body(&body)?;
    Ok(models)
}

fn parse_models_body(body: &Value) -> Result<Vec<String>, String> {
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
        return Err(
            "response had no recognizable model list (expected {\"data\":[{\"id\":...}]})".into(),
        );
    }
    Ok(models)
}

pub(crate) async fn fetch_commandcode_models(
    http: &reqwest::Client,
) -> Result<Vec<String>, String> {
    let url = std::env::var("ROUTER_COMMANDCODE_MODELS_URL")
        .unwrap_or_else(|_| commandcode::DEFAULT_MODELS_URL.to_string());
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("HTTP {}: {snippet}", status.as_u16()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("could not parse response as JSON: {e}"))?;
    parse_models_body(&body)
}

/// Fetches and caches a provider's live model list in `state.discovered_models`
/// so `GET /v1/models` can list `<provider_id>/<model>` entries without a
/// network call of its own. Called both from the explicit `list-models`
/// endpoint below and, best-effort, right after a provider is created.
/// Best-effort only: Codex OAuth has no discoverable models endpoint, while
/// Command Code uses its fixed unauthenticated provider endpoint; passthrough
/// mirrors may also omit `/models`. These cases are reported as an `Err`
/// reason rather than panicking or retrying.
pub(crate) async fn discover_and_cache_models(
    state: &AppState,
    provider: &Provider,
) -> Result<Vec<String>, String> {
    let models = match provider.kind {
        ProviderKind::Passthrough => fetch_live_models(&state.http, provider).await?,
        ProviderKind::OauthCommandCode => fetch_commandcode_models(&state.http).await?,
        ProviderKind::OauthCodex => {
            return Err("this provider kind has no discoverable /models endpoint".into())
        }
    };
    state
        .discovered_models
        .insert(provider.id.clone(), models.clone());
    Ok(models)
}

/// Discovery timeout for background probes the caller never explicitly
/// asked for (provider creation, startup warm-up) - independent of the
/// shared http client's own connect/idle timeouts, which are tuned for real
/// proxy traffic and can run 10s+. A bad/placeholder base_url shouldn't tie
/// up a background task anywhere near that long.
const BACKGROUND_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn spawn_bounded_discovery(state: AppState, provider: Provider) {
    if provider.kind == ProviderKind::OauthCodex {
        return;
    }
    tokio::spawn(async move {
        let _ = tokio::time::timeout(
            BACKGROUND_DISCOVERY_TIMEOUT,
            discover_and_cache_models(&state, &provider),
        )
        .await;
    });
}

/// Warms the discovered-models cache for every existing discoverable provider
/// at boot, so `GET /v1/models` reflects providers that were
/// created before this feature existed (or before the process last
/// restarted, since the cache is in-memory only) without requiring each one
/// to be manually re-fetched or recreated. Fire-and-forget per provider,
/// same as the auto-fetch on creation - never blocks startup.
pub fn warm_discovered_models_cache(state: &AppState) {
    let snapshot = state.snapshot.load();
    for provider in snapshot.providers.iter() {
        spawn_bounded_discovery(state.clone(), provider.clone());
    }
}

async fn list_models(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    match discover_and_cache_models(&s, &provider).await {
        Ok(models) => Ok(Json(json!({ "ok": true, "models": models }))),
        Err(reason) => Ok(Json(json!({ "ok": false, "reason": reason }))),
    }
}

#[derive(Deserialize)]
struct ListModelsPreviewBody {
    wire_format: WireFormat,
    base_url: String,
    api_key: Option<String>,
}

/// "Fetch models" for the create form, before a provider row (and thus an
/// id to cache discovered models under) exists yet. Passthrough-only, same
/// reasoning as `validate_model_preview`.
async fn list_models_preview(
    State(s): State<AppState>,
    Json(body): Json<ListModelsPreviewBody>,
) -> Result<Json<Value>, AppError> {
    let now = Utc::now();
    let probe = Provider {
        id: String::new(),
        name: String::new(),
        wire_format: body.wire_format,
        kind: ProviderKind::Passthrough,
        base_url: Some(body.base_url),
        api_key: body.api_key,
        upstream_model: String::new(),
        created_at: now,
        updated_at: now,
    };
    match fetch_live_models(&s.http, &probe).await {
        Ok(models) => Ok(Json(json!({ "ok": true, "models": models }))),
        Err(reason) => Ok(Json(json!({ "ok": false, "reason": reason }))),
    }
}

async fn state_stub(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    queries::get_provider(&s.db, &id).await?;
    // A provider can back several models (each its own runtime_key, see
    // migrations/0005_pool_member_model_identity.sql) - show the worst
    // status across all of them, so one provider row still shows one
    // status in the admin UI.
    let entry = crate::core::runtime::worst_provider_state(&s.runtime, &id);
    let (level, status, until_secs) = match entry {
        Some(st) => {
            let secs = st.unavailable_until.map(|u| {
                u.saturating_duration_since(std::time::Instant::now())
                    .as_secs()
            });
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
    use super::{derive_models_url, parse_models_body};
    use serde_json::json;

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

    #[test]
    fn derive_models_url_handles_a_base_url_with_no_path_at_all() {
        // Regression: a naive `rfind('/')` matches the slash inside the
        // scheme's `//` separator here and produces the nonsense host
        // `http://models` instead of appending a path.
        assert_eq!(
            derive_models_url("http://127.0.0.1:1"),
            "http://127.0.0.1:1/models"
        );
        assert_eq!(
            derive_models_url("https://api.example.com"),
            "https://api.example.com/models"
        );
    }

    #[test]
    fn parse_models_payload_ignores_extra_fields() {
        let body =
            json!({"object":"list","data":[{"id":"cc-1","name":"CC One","context_length":200000}]});
        assert_eq!(parse_models_body(&body).unwrap(), vec!["cc-1"]);
    }
}
