use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::core::error::AppError;
use crate::core::model::ProviderKind;
use crate::core::state::{reload_snapshot, AppState};
use crate::providers::adapter::codex::oauth;
use crate::providers::queries;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/providers/:id/oauth/start", post(start))
        .route("/admin/providers/:id/oauth/complete", post(complete))
}

async fn start(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
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
}

async fn complete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CompleteBody>,
) -> Result<Json<Value>, AppError> {
    let os = queries::get_oauth_state(&s.db, &id)
        .await?
        .ok_or_else(|| AppError::BadRequest("no oauth flow in progress; call start first".into()))?;
    let verifier = os
        .pkce_verifier
        .ok_or_else(|| AppError::BadRequest("missing pkce verifier".into()))?;

    let tokens = oauth::exchange_code(&s.http, &b.code, &verifier)
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
        &s.db,
        &id,
        Some(&tokens.access_token),
        tokens.refresh_token.as_deref(),
        tokens.id_token.as_deref(),
        expires_at,
        &provider_data,
    )
    .await?;
    queries::clear_pkce(&s.db, &id).await?;
    reload_snapshot(&s).await?;

    Ok(Json(json!({ "status": "ok" })))
}
