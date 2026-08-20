use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::core::error::AppError;
use crate::core::model::ProviderKind;
use crate::core::state::{reload_snapshot, AppState};
use crate::onboarding::store_commandcode_key;
use crate::providers::adapter::codex::oauth;
use crate::providers::adapter::commandcode::browser_login::{self, AuthListener, LoginError};
use crate::providers::queries;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/providers/:id/oauth/start", post(start))
        .route("/admin/providers/:id/oauth/complete", post(complete))
        .route(
            "/admin/providers/:id/commandcode/key",
            post(set_commandcode_key),
        )
        .route(
            "/admin/providers/:id/commandcode/browser-login/start",
            post(start_commandcode_browser_login),
        )
        .route(
            "/admin/providers/:id/commandcode/browser-login/status",
            get(commandcode_browser_login_status),
        )
}

async fn start(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    if !matches!(provider.kind, ProviderKind::OauthCodex) {
        return Err(AppError::BadRequest("provider is not oauth_codex".into()));
    }
    let pkce = oauth::generate_pkce();
    let state_tok = Uuid::new_v4().to_string();
    queries::store_pkce(&s.db, &id, &pkce.verifier, &state_tok).await?;
    let url = oauth::build_authorize_url(&state_tok, &pkce.challenge);
    Ok(Json(json!({ "authorize_url": url })))
}

#[derive(Deserialize)]
struct CompleteBody {
    code: String,
    // Required: the `state` param from the same redirect URL the `code` came
    // from. Validated against what /oauth/start issued for this provider -
    // without this, `state` is stored but never checked, so it provides no
    // real CSRF binding (found in the Phase 3 review).
    state: String,
}

/// Validate `state`, exchange `code`, persist tokens, clear the PKCE row.
///
/// Extracted from the `complete` handler so the onboarding wizard can run the
/// exact same exchange in-process (no HTTP hop) instead of duplicating it.
pub async fn complete_oauth_exchange(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    provider_id: &str,
    code: &str,
    state: &str,
) -> Result<(), AppError> {
    let os = queries::get_oauth_state(db, provider_id)
        .await?
        .ok_or_else(|| {
            AppError::BadRequest("no oauth flow in progress; call start first".into())
        })?;
    let verifier = os
        .pkce_verifier
        .ok_or_else(|| AppError::BadRequest("missing pkce verifier".into()))?;
    let expected_state = os
        .oauth_state
        .ok_or_else(|| AppError::BadRequest("missing oauth state; call start first".into()))?;
    if state != expected_state {
        return Err(AppError::BadRequest("state mismatch".into()));
    }

    let tokens = oauth::exchange_code(http, code, &verifier)
        .await
        .map_err(|e| AppError::BadRequest(format!("code exchange failed: {e}")))?;

    let mut provider_data = serde_json::json!({});
    if let Some(idt) = &tokens.id_token {
        let claims = oauth::decode_account_claims(idt);
        if let Some(acct) = claims.chatgpt_account_id {
            provider_data["chatgpt_account_id"] = json!(acct);
        }
        if let Some(ws) = claims.workspace_id {
            provider_data["workspace_id"] = json!(ws);
        }
    }
    let expires_at = tokens
        .expires_in
        .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s));

    queries::upsert_oauth_tokens(
        db,
        provider_id,
        Some(&tokens.access_token),
        tokens.refresh_token.as_deref(),
        tokens.id_token.as_deref(),
        expires_at,
        &provider_data,
    )
    .await?;
    queries::clear_pkce(db, provider_id).await?;
    Ok(())
}

async fn complete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CompleteBody>,
) -> Result<Json<Value>, AppError> {
    complete_oauth_exchange(&s.db, &s.http, &id, &b.code, &b.state).await?;
    reload_snapshot(&s).await?;
    Ok(Json(json!({ "status": "ok" })))
}

#[derive(Deserialize)]
struct CommandCodeKeyBody {
    /// May be empty to mean "use the key found on disk" (see below).
    api_key: String,
}

async fn set_commandcode_key(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CommandCodeKeyBody>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    if provider.kind != ProviderKind::OauthCommandCode {
        return Err(AppError::BadRequest(
            "provider is not oauth_command_code".into(),
        ));
    }
    let key = body.api_key.trim();
    if key.is_empty() {
        let from_disk = crate::providers::adapter::commandcode::api_key::commandcode_key_from_disk();
        let key = from_disk
            .ok_or_else(|| AppError::BadRequest("api_key must not be empty".into()))?;
        store_commandcode_key(&s.db, &id, &key).await?;
    } else {
        store_commandcode_key(&s.db, &id, key).await?;
    }
    reload_snapshot(&s).await?;
    // A fresh key can fix whatever previously flagged the provider
    // misconfigured (e.g. a 401) - clear the stale runtime flag so the proxy
    // path tries it again without a restart.
    if let Some(mut st) = s.runtime.get_mut(&id) {
        st.reset_to_healthy();
    }
    Ok(Json(json!({ "ok": true })))
}

/// Status of an in-flight admin-UI Command Code browser login, keyed by
/// provider id. Deliberately a process-global map rather than an `AppState`
/// field: the login itself is a short-lived background task started by one
/// HTTP request and polled by later ones, not durable app state, and this
/// avoids touching every `AppState { .. }` construction site (tests
/// included) for a field only these two handlers need.
#[derive(Clone)]
enum CommandCodeLoginStatus {
    Pending,
    Success,
    Error(String),
}

fn commandcode_logins() -> &'static DashMap<String, CommandCodeLoginStatus> {
    static MAP: OnceLock<DashMap<String, CommandCodeLoginStatus>> = OnceLock::new();
    MAP.get_or_init(DashMap::new)
}

/// Bind the same localhost callback listener the CLI wizard uses
/// (`onboarding::add_commandcode_provider`) and open its authorize URL for
/// the admin UI to `window.open`. Only works when the browser completing the
/// commandcode.ai login runs on the same host as this server, since
/// commandcode.ai's page posts the result to `http://localhost:<port>` from
/// the browser's own machine - true for the common self-hosted case where
/// the admin UI is opened on the machine running 1router, not for a remote
/// admin UI accessed over the network.
async fn start_commandcode_browser_login(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let provider = queries::get_provider(&s.db, &id).await?;
    if provider.kind != ProviderKind::OauthCommandCode {
        return Err(AppError::BadRequest(
            "provider is not oauth_command_code".into(),
        ));
    }
    let (listener, port) = browser_login::bind_listener()
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to bind callback listener: {e}")))?;
    let state_token = Uuid::new_v4().to_string();
    let auth = AuthListener::new(listener, port, state_token.clone());
    let url = auth.authorize_url();

    commandcode_logins().insert(id.clone(), CommandCodeLoginStatus::Pending);

    let db = s.db.clone();
    let provider_id = id.clone();
    tokio::spawn(async move {
        let outcome = match auth.wait().await {
            Ok(callback) => browser_login::validate_state(&state_token, callback)
                .map_err(|e| format!("{e:?}"))
                .map(|callback| callback.api_key),
            Err(LoginError::Denied(reason)) => Err(format!("login denied: {reason}")),
            Err(LoginError::Timeout) => Err("login timed out".to_string()),
            Err(error) => Err(format!("{error:?}")),
        };
        let status = match outcome {
            Ok(key) => match store_commandcode_key(&db, &provider_id, &key).await {
                Ok(()) => CommandCodeLoginStatus::Success,
                Err(e) => CommandCodeLoginStatus::Error(format!("failed to store key: {e}")),
            },
            Err(message) => CommandCodeLoginStatus::Error(message),
        };
        commandcode_logins().insert(provider_id, status);
    });

    Ok(Json(json!({ "authorize_url": url })))
}

async fn commandcode_browser_login_status(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let status = commandcode_logins().get(&id).map(|entry| entry.clone());
    let succeeded = matches!(status, Some(CommandCodeLoginStatus::Success));
    let terminal = succeeded || matches!(status, Some(CommandCodeLoginStatus::Error(_)));
    let body = match status {
        None => json!({ "status": "not_started" }),
        Some(CommandCodeLoginStatus::Pending) => json!({ "status": "pending" }),
        Some(CommandCodeLoginStatus::Success) => json!({ "status": "success" }),
        Some(CommandCodeLoginStatus::Error(message)) => {
            json!({ "status": "error", "error": message })
        }
    };
    if terminal {
        commandcode_logins().remove(&id);
    }
    if succeeded {
        reload_snapshot(&s).await?;
    }
    Ok(Json(body))
}
