use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{DatasetLogEntry, LatencyMs, LogEntry, Provider, ProviderKind, WireFormat};
use crate::core::runtime::runtime_key;
use crate::core::state::AppState;
use crate::pools::select::{
    dataset_logging_enabled, direct_model_allowed, resolve_reasoning_effort, select,
};
use crate::providers::adapter::commandcode::{
    current_transport, is_upgrade_required, remember_transport, Transport,
};
use crate::providers::adapter::{adapter_for_wire, Credentials};
use crate::providers::queries::get_oauth_state;
use crate::providers::refresh_lock::refresh_and_persist_detached;
use crate::proxy::backoff;
use crate::proxy::dataset_tee;
use crate::proxy::error_response::wire_error;
use crate::users::Caller;

pub(crate) async fn credentials_for(state: &AppState, provider: &Provider) -> Credentials {
    Credentials::from_provider_and_oauth(
        provider,
        get_oauth_state(&state.db, &provider.id).await.ok().flatten(),
    )
}

fn log(
    state: &AppState,
    caller: &Caller,
    pool_id: &str,
    provider_id: &str,
    status: Option<i64>,
    latency_ms: i64,
    success: bool,
) {
    // Logging must never block the hot path.
    let _ = state.log_tx.try_send(LogEntry {
        pool_id: Some(pool_id.to_string()),
        provider_id: Some(provider_id.to_string()),
        status_code: status,
        latency_ms,
        success,
        user_id: caller.user_id.clone(),
        modality: None,
        units: None,
    });
}

/// The dataset-logging tap. When `enabled` is `false` (the common case),
/// this is a no-op: `resp` is returned untouched, no accumulator is
/// allocated, no body clone happens. When `enabled`, wraps `resp`'s body
/// with `dataset_tee::tee` so the client still sees every byte unchanged
/// while a full copy is accumulated and, once the response ends (cleanly,
/// on an upstream error mid-stream, or on a client disconnect - see
/// `dataset_tee`'s `FireOnDrop` guard), written via
/// `state.dataset_log_tx.try_send` - never blocking, dropped on a full
/// channel, exactly like `log()` above.
#[allow(clippy::too_many_arguments)]
fn maybe_log_dataset(
    state: &AppState,
    enabled: bool,
    resp: Response,
    caller: &Caller,
    pool_id: Option<String>,
    provider_id: String,
    model: String,
    wire: WireFormat,
    stream: bool,
    body: &Bytes,
    total_start: Instant,
    ttfb_ms: Option<i64>,
) -> Response {
    if !enabled {
        return resp;
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now();
    let input_body = String::from_utf8_lossy(body).into_owned();
    let dataset_log_tx = state.dataset_log_tx.clone();
    let user_id = caller.user_id.clone();

    let (parts, resp_body) = resp.into_parts();
    let wrapped = dataset_tee::tee(resp_body, move |output_bytes, complete| {
        let entry = DatasetLogEntry {
            request_id,
            timestamp,
            pool_id,
            provider_id,
            model,
            user_id,
            wire_format: wire,
            stream,
            input_body,
            output_body: String::from_utf8_lossy(&output_bytes).into_owned(),
            complete,
            latency_ms: LatencyMs {
                ttfb_ms,
                total_ms: total_start.elapsed().as_millis() as i64,
            },
        };
        let _ = dataset_log_tx.try_send(entry);
    });
    Response::from_parts(parts, wrapped)
}

pub async fn handle_proxy(
    state: AppState,
    wire: WireFormat,
    pool_id: String,
    caller: Caller,
    body: Bytes,
) -> Response {
    let is_admin = caller.is_admin();
    let mut resp = handle_proxy_inner(state, wire, pool_id, caller, body).await;
    if !is_admin {
        strip_debug_headers(resp.headers_mut());
    }
    resp
}

/// `x-1router-tried` / `-provider` / `-error` expose provider ids and
/// internal error text - routing topology is the admin's business, not every
/// caller's (SEC-11). Only the shared-secret admin gets them.
fn strip_debug_headers(headers: &mut HeaderMap) {
    for name in ["x-1router-tried", "x-1router-provider", "x-1router-error"] {
        headers.remove(name);
    }
}

async fn handle_proxy_inner(
    state: AppState,
    wire: WireFormat,
    pool_id: String,
    caller: Caller,
    body: Bytes,
) -> Response {
    let snapshot = state.snapshot.load();
    let selection = match select(&snapshot, &pool_id, wire, &state.pool_rotation) {
        Some(s) => s,
        None => {
            return wire_error(
                wire,
                StatusCode::BAD_REQUEST,
                &format!("unknown model or pool '{pool_id}'"),
            );
        }
    };

    if selection.pool.is_none() && !caller.is_admin() {
        if let Some(member) = selection.providers.first() {
            if !direct_model_allowed(&snapshot, &state.discovered_models, member.provider, &member.effective_model) {
                return wire_error(
                    wire,
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "model '{}' is not enabled for direct addressing on provider '{}'",
                        member.effective_model, member.provider.id
                    ),
                );
            }
        }
    }

    let client_wanted_stream = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    // For dataset logging's `latency_ms.total_ms` (see `maybe_log_dataset`)
    // - captured once, unconditionally, before the failover loop, since it
    // needs to cover every attempt's time, not just the winning one's.
    let total_start = Instant::now();

    let mut tried: Vec<String> = Vec::new();
    let mut last_error_body = String::from("no provider produced a response");
    let mut last_provider = String::new();

    for member in &selection.providers {
        let provider = member.provider;
        let effective_model = &member.effective_model;
        let member_override = &member.dataset_logging_override;
        let now = Instant::now();
        // `get`, not `entry`: an untried (provider, model) has no state yet
        // and mustn't gain an entry just for being looked up (SEC-03).
        if let Some(st) = state.runtime.get(&runtime_key(&provider.id, effective_model)) {
            if !st.is_available(now) {
                continue;
            }
        }
        tried.push(provider.id.clone());
        last_provider = provider.id.clone();

        // Adapters read `provider.upstream_model` directly; route the
        // pool-member's effective model (its override, or the provider's own
        // default) through a cheap per-request clone rather than threading it
        // through the ProviderAdapter trait.
        // Adapters also read `provider.default_reasoning_effort` directly;
        // fold the member's override into the same clone rather than
        // widening `ProviderAdapter::build_request`'s signature.
        let provider = &Provider {
            upstream_model: effective_model.clone(),
            default_reasoning_effort: resolve_reasoning_effort(
                provider,
                member.reasoning_effort_override,
            ),
            ..(*provider).clone()
        };

        let adapter: std::sync::Arc<dyn crate::providers::adapter::ProviderAdapter> =
            adapter_for_wire(provider, state.http.clone(), wire).into();
        let creds = credentials_for(&state, provider).await;

        let req = match adapter.build_request(&body, &creds).await {
            Ok(r) => r,
            Err(e) => {
                last_error_body = format!("request build failed: {e}");
                continue;
            }
        };

        let start = Instant::now();
        let sent = state.http.execute(req).await;
        let latency_ms = start.elapsed().as_millis() as i64;

        let upstream = match sent {
            Ok(r) => r,
            Err(e) => {
                {
                    let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                    let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                    st.record_retryable(cooldown, Instant::now());
                }
                log(&state, &caller, &pool_id, &provider.id, None, latency_ms, false);
                tracing::warn!(provider = %provider.id, error = %e, "upstream request error");
                last_error_body = format!("upstream request error: {}", e.without_url());
                continue;
            }
        };

        let status = upstream.status();
        let headers = upstream.headers().clone();
        let class = adapter.classify_error(status, &headers).await;

        match class {
            ErrorClass::Success => {
                {
                    let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                    st.record_success();
                }
                match adapter
                    .transform_response(upstream, client_wanted_stream)
                    .await
                {
                    Ok(resp) => {
                        log(
                            &state,
                            &caller,
                            &pool_id,
                            &provider.id,
                            Some(status.as_u16() as i64),
                            latency_ms,
                            true,
                        );
                        let dataset_enabled = dataset_logging_enabled(provider, *member_override);
                        return maybe_log_dataset(
                            &state,
                            dataset_enabled,
                            resp,
                            &caller,
                            selection.pool.map(|p| p.id.clone()),
                            provider.id.clone(),
                            effective_model.clone(),
                            wire,
                            client_wanted_stream,
                            &body,
                            total_start,
                            Some(latency_ms),
                        );
                    }
                    Err(e) => {
                        // The upstream HTTP status was a success, but the
                        // body embedded an error (e.g. commandcode's
                        // ndjson error events) — log the real outcome, not
                        // the misleading raw status.
                        log(
                            &state,
                            &caller,
                            &pool_id,
                            &provider.id,
                            Some(status.as_u16() as i64),
                            latency_ms,
                            false,
                        );
                        // An embedded error with a known HTTP status (e.g.
                        // commandcode's `<400>` prefix on a bad vision
                        // request) should reach the client as that status,
                        // not a generic 503.
                        if let AppError::UpstreamWithStatus(s, msg) = &e {
                            return build_wire_error(wire, *s, msg, &tried, &provider.id);
                        }
                        last_error_body = format!("response transform failed: {e}");
                        continue;
                    }
                }
            }
            ErrorClass::NonRetryable => {
                // Client-caused rejection: no runtime-state change (SEC-01).
                let content_type = headers.get(axum::http::header::CONTENT_TYPE).cloned();
                let text = crate::core::http_client::read_text_truncated(upstream, crate::core::http_client::MAX_ERROR_BODY).await;
                log(
                    &state,
                    &caller,
                    &pool_id,
                    &provider.id,
                    Some(status.as_u16() as i64),
                    latency_ms,
                    false,
                );
                return build_error_passthrough(status, &text, &tried, &provider.id, content_type);
            }
            ErrorClass::AuthExpired => {
                // Only oauth_codex can recover via refresh; others are misconfigured.
                if !matches!(provider.kind, ProviderKind::OauthCodex)
                    || creds.refresh_token.is_none()
                {
                    {
                        let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                        st.mark_misconfigured(Instant::now());
                    }
                    let content_type = headers.get(axum::http::header::CONTENT_TYPE).cloned();
                    let text = crate::core::http_client::read_text_truncated(upstream, crate::core::http_client::MAX_ERROR_BODY).await;
                    log(
                        &state,
                        &caller,
                        &pool_id,
                        &provider.id,
                        Some(status.as_u16() as i64),
                        latency_ms,
                        false,
                    );
                    return build_error_passthrough(
                        status,
                        &text,
                        &tried,
                        &provider.id,
                        content_type,
                    );
                }
                drop(upstream);
                // Detached so a client disconnect can't drop the rotated
                // refresh token between the upstream call and the DB write.
                let refreshed =
                    refresh_and_persist_detached(&state, provider, adapter.clone(), &creds).await;
                match refreshed {
                    Ok(new_creds) => {
                        // Retry the same provider once with new credentials.
                        let retry_req = match adapter.build_request(&body, &new_creds).await {
                            Ok(req) => req,
                            Err(e) => {
                                log(&state, &caller, &pool_id, &provider.id, None, 0, false);
                                last_error_body = format!("retry request build failed: {e}");
                                continue;
                            }
                        };
                        let start2 = Instant::now();
                        let resp2 = match state.http.execute(retry_req).await {
                            Ok(resp) => resp,
                            Err(e) => {
                                let lat2 = start2.elapsed().as_millis() as i64;
                                {
                                    let mut st =
                                        state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                    let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                                    st.record_retryable(cooldown, Instant::now());
                                }
                                log(&state, &caller, &pool_id, &provider.id, None, lat2, false);
                                tracing::warn!(provider = %provider.id, error = %e, "retry upstream request error");
                last_error_body = format!("retry upstream request error: {}", e.without_url());
                                continue;
                            }
                        };
                        let lat2 = start2.elapsed().as_millis() as i64;
                        let retry_status = resp2.status();
                        let retry_headers = resp2.headers().clone();
                        let retry_class = adapter
                            .classify_error(retry_status, &retry_headers)
                            .await;

                        match retry_class {
                            ErrorClass::Success => {
                                {
                                    let mut st =
                                        state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                    st.record_success();
                                }
                                match adapter
                                    .transform_response(resp2, client_wanted_stream)
                                    .await
                                {
                                    Ok(response) => {
                                        log(
                                            &state,
                                            &caller,
                                            &pool_id,
                                            &provider.id,
                                            Some(retry_status.as_u16() as i64),
                                            lat2,
                                            true,
                                        );
                                        let dataset_enabled =
                                            dataset_logging_enabled(provider, *member_override);
                                        return maybe_log_dataset(
                                            &state,
                                            dataset_enabled,
                                            response,
                                            &caller,
                                            selection.pool.map(|p| p.id.clone()),
                                            provider.id.clone(),
                                            effective_model.clone(),
                                            wire,
                                            client_wanted_stream,
                                            &body,
                                            total_start,
                                            Some(lat2),
                                        );
                                    }
                                    Err(e) => {
                                        log(
                                            &state,
                                            &caller,
                                            &pool_id,
                                            &provider.id,
                                            Some(retry_status.as_u16() as i64),
                                            lat2,
                                            false,
                                        );
                                        if let AppError::UpstreamWithStatus(s, msg) = &e {
                                            return build_wire_error(
                                                wire,
                                                *s,
                                                msg,
                                                &tried,
                                                &provider.id,
                                            );
                                        }
                                        last_error_body = format!(
                                            "retry response transform failed: {e}"
                                        );
                                    }
                                }
                            }
                            ErrorClass::NonRetryable => {
                                // Client-caused rejection: no runtime-state change (SEC-01).
                                let text = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                                log(
                                    &state,
                                    &caller,
                                    &pool_id,
                                    &provider.id,
                                    Some(retry_status.as_u16() as i64),
                                    lat2,
                                    false,
                                );
                                last_error_body = text;
                            }
                            ErrorClass::AuthExpired => {
                                {
                                    let mut st =
                                        state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                    st.mark_misconfigured(Instant::now());
                                }
                                let text = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                                log(
                                    &state,
                                    &caller,
                                    &pool_id,
                                    &provider.id,
                                    Some(retry_status.as_u16() as i64),
                                    lat2,
                                    false,
                                );
                                last_error_body = text;
                            }
                            ErrorClass::Retryable { retry_after } => {
                                let cooldown = retry_after.unwrap_or_else(|| {
                                    if retry_status.is_server_error()
                                        || retry_status == StatusCode::TOO_MANY_REQUESTS
                                        || retry_status == StatusCode::REQUEST_TIMEOUT
                                    {
                                        let st =
                                            state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                        backoff::cooldown_for(st.backoff_level + 1)
                                    } else {
                                        Duration::from_secs(30)
                                    }
                                });
                                {
                                    let mut st =
                                        state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                    st.record_retryable(cooldown, Instant::now());
                                }
                                last_error_body = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                                log(
                                    &state,
                                    &caller,
                                    &pool_id,
                                    &provider.id,
                                    Some(retry_status.as_u16() as i64),
                                    lat2,
                                    false,
                                );
                            }
                        }
                        // Each retry_class arm above already set last_error_body to the
                        // real failure text; don't clobber it with a generic message.
                        continue;
                    }
                    Err(RefreshError::InvalidGrant) => {
                        {
                            let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                            st.mark_misconfigured(Instant::now());
                        }
                        last_error_body = "refresh token invalid_grant; re-auth required".into();
                        log(&state, &caller, &pool_id, &provider.id, Some(401), latency_ms, false);
                        continue;
                    }
                    Err(RefreshError::Transient(msg)) => {
                        {
                            let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                            let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                            st.record_retryable(cooldown, Instant::now());
                        }
                        last_error_body = format!("transient refresh error: {msg}");
                        log(&state, &caller, &pool_id, &provider.id, Some(401), latency_ms, false);
                        continue;
                    }
                }
            }
            ErrorClass::Retryable { retry_after } => {
                let error_text = crate::core::http_client::read_text_truncated(upstream, crate::core::http_client::MAX_ERROR_BODY).await;

                // Command Code transport fallback: a 403 with
                // `upgrade_required` from the provider transport means this
                // account must use `/alpha/generate` (Go-plan accounts - see
                // pi's transport.ts). Remember it and retry the same provider
                // once through the generate transport before the normal
                // cooldown path.
                if provider.kind == ProviderKind::OauthCommandCode
                    && current_transport(&provider.id) == Transport::Provider
                    && is_upgrade_required(status, &error_text)
                {
                    remember_transport(&provider.id, Transport::Generate);
                    let retry_req = match adapter.build_request(&body, &creds).await {
                        Ok(req) => req,
                        Err(e) => {
                            log(&state, &caller, &pool_id, &provider.id, None, 0, false);
                            last_error_body = format!("retry request build failed: {e}");
                            continue;
                        }
                    };
                    let start2 = Instant::now();
                    let resp2 = match state.http.execute(retry_req).await {
                        Ok(resp) => resp,
                        Err(e) => {
                            let lat2 = start2.elapsed().as_millis() as i64;
                            {
                                let mut st =
                                    state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                                st.record_retryable(cooldown, Instant::now());
                            }
                            log(&state, &caller, &pool_id, &provider.id, None, lat2, false);
                            tracing::warn!(provider = %provider.id, error = %e, "retry upstream request error");
                last_error_body = format!("retry upstream request error: {}", e.without_url());
                            continue;
                        }
                    };
                    let lat2 = start2.elapsed().as_millis() as i64;
                    let retry_status = resp2.status();
                    let retry_headers = resp2.headers().clone();
                    let retry_class = adapter
                        .classify_error(retry_status, &retry_headers)
                        .await;
                    match retry_class {
                        ErrorClass::Success => {
                            {
                                let mut st =
                                    state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                st.record_success();
                            }
                            match adapter
                                .transform_response(resp2, client_wanted_stream)
                                .await
                            {
                                Ok(response) => {
                                    log(
                                        &state,
                                        &caller,
                                        &pool_id,
                                        &provider.id,
                                        Some(retry_status.as_u16() as i64),
                                        lat2,
                                        true,
                                    );
                                    let dataset_enabled =
                                        dataset_logging_enabled(provider, *member_override);
                                    return maybe_log_dataset(
                                        &state,
                                        dataset_enabled,
                                        response,
                                        &caller,
                                        selection.pool.map(|p| p.id.clone()),
                                        provider.id.clone(),
                                        effective_model.clone(),
                                        wire,
                                        client_wanted_stream,
                                        &body,
                                        total_start,
                                        Some(lat2),
                                    );
                                }
                                Err(e) => {
                                    log(
                                        &state,
                                        &caller,
                                        &pool_id,
                                        &provider.id,
                                        Some(retry_status.as_u16() as i64),
                                        lat2,
                                        false,
                                    );
                                    if let AppError::UpstreamWithStatus(s, msg) = &e {
                                        return build_wire_error(
                                            wire,
                                            *s,
                                            msg,
                                            &tried,
                                            &provider.id,
                                        );
                                    }
                                    last_error_body =
                                        format!("retry response transform failed: {e}");
                                }
                            }
                        }
                        ErrorClass::NonRetryable => {
                            // Client-caused rejection: no runtime-state change (SEC-01).
                            last_error_body = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                            log(
                                &state,
                                &caller,
                                &pool_id,
                                &provider.id,
                                Some(retry_status.as_u16() as i64),
                                lat2,
                                false,
                            );
                        }
                        ErrorClass::AuthExpired => {
                            {
                                let mut st =
                                    state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                st.mark_misconfigured(Instant::now());
                            }
                            last_error_body = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                            log(
                                &state,
                                &caller,
                                &pool_id,
                                &provider.id,
                                Some(retry_status.as_u16() as i64),
                                lat2,
                                false,
                            );
                        }
                        ErrorClass::Retryable { .. } => {
                            let cooldown = {
                                let st =
                                    state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                backoff::cooldown_for(st.backoff_level + 1)
                            };
                            {
                                let mut st =
                                    state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                                st.record_retryable(cooldown, Instant::now());
                            }
                            last_error_body = crate::core::http_client::read_text_truncated(resp2, crate::core::http_client::MAX_ERROR_BODY).await;
                            log(
                                &state,
                                &caller,
                                &pool_id,
                                &provider.id,
                                Some(retry_status.as_u16() as i64),
                                lat2,
                                false,
                            );
                        }
                    }
                    continue;
                }

                let cooldown = retry_after.unwrap_or_else(|| {
                    if status.is_server_error()
                        || status == StatusCode::TOO_MANY_REQUESTS
                        || status == StatusCode::REQUEST_TIMEOUT
                    {
                        let st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                        backoff::cooldown_for(st.backoff_level + 1)
                    } else {
                        Duration::from_secs(30)
                    }
                });
                {
                    let mut st = state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default();
                    st.record_retryable(cooldown, Instant::now());
                }
                last_error_body = error_text;
                log(
                    &state,
                    &caller,
                    &pool_id,
                    &provider.id,
                    Some(status.as_u16() as i64),
                    latency_ms,
                    false,
                );
                continue;
            }
        }
    }

    let mut resp = wire_error(wire, StatusCode::SERVICE_UNAVAILABLE, &last_error_body);
    insert_debug_headers(resp.headers_mut(), &tried, &last_provider, &last_error_body);
    resp
}

fn build_error_passthrough(
    status: StatusCode,
    body: &str,
    tried: &[String],
    provider_id: &str,
    content_type: Option<HeaderValue>,
) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    // Preserve the upstream's content-type (e.g. application/json) instead of the
    // text/plain that (StatusCode, String) sets by default, so SDK clients parsing
    // the relayed error body don't misinterpret it.
    if let Some(ct) = content_type {
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, ct);
    }
    insert_debug_headers(resp.headers_mut(), tried, provider_id, body);
    resp
}

/// Relay an embedded upstream error whose HTTP status we know in the
/// client's wire shape (OpenAI/Anthropic JSON), with the debug headers.
fn build_wire_error(
    wire: WireFormat,
    status: StatusCode,
    body: &str,
    tried: &[String],
    provider_id: &str,
) -> Response {
    let mut resp = wire_error(wire, status, body);
    insert_debug_headers(resp.headers_mut(), tried, provider_id, body);
    resp
}

fn insert_debug_headers(headers: &mut HeaderMap, tried: &[String], provider: &str, error: &str) {
    if let Ok(v) = HeaderValue::from_str(&tried.join(",")) {
        headers.insert("x-1router-tried", v);
    }
    if let Ok(v) = HeaderValue::from_str(provider) {
        headers.insert("x-1router-provider", v);
    }
    let short: String = error.chars().take(200).collect();
    if let Ok(v) = HeaderValue::from_str(&short.replace(['\n', '\r'], " ")) {
        headers.insert("x-1router-error", v);
    }
}

#[cfg(test)]
mod tests {
    use crate::core::runtime::{ProviderRuntimeState, ProviderStatus};
    use std::time::Instant;

    #[test]
    fn misconfigured_is_skipped() {
        let mut st = ProviderRuntimeState::default();
        let now = Instant::now();
        st.mark_misconfigured(now);
        assert!(!st.is_available(now));
        assert!(matches!(st.status, ProviderStatus::Misconfigured));
    }
}
