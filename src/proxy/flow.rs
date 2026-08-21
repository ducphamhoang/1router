use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{LogEntry, Provider, ProviderKind, WireFormat};
use crate::core::state::AppState;
use crate::pools::select::select;
use crate::providers::adapter::commandcode::{
    current_transport, is_upgrade_required, remember_transport, Transport,
};
use crate::providers::adapter::{adapter_for_wire, Credentials};
use crate::providers::queries::get_oauth_state;
use crate::providers::refresh_lock::{refresh_and_persist, with_refresh_lock};
use crate::proxy::backoff;
use crate::proxy::error_response::wire_error;

pub(crate) async fn credentials_for(state: &AppState, provider: &Provider) -> Credentials {
    Credentials::from_provider_and_oauth(
        provider,
        get_oauth_state(&state.db, &provider.id).await.ok().flatten(),
    )
}

fn log(
    state: &AppState,
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
    });
}

pub async fn handle_proxy(
    state: AppState,
    wire: WireFormat,
    pool_id: String,
    _client_headers: HeaderMap,
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

    let client_wanted_stream = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    let mut tried: Vec<String> = Vec::new();
    let mut last_error_body = String::from("no provider produced a response");
    let mut last_provider = String::new();

    for (provider, effective_model) in &selection.providers {
        let now = Instant::now();
        {
            let st = state.runtime.entry(provider.id.clone()).or_default();
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
        let provider = &Provider {
            upstream_model: effective_model.clone(),
            ..(*provider).clone()
        };

        let adapter = adapter_for_wire(provider, state.http.clone(), wire);
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
                    let mut st = state.runtime.entry(provider.id.clone()).or_default();
                    let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                    st.record_retryable(cooldown, Instant::now());
                }
                log(&state, &pool_id, &provider.id, None, latency_ms, false);
                last_error_body = format!("upstream request error: {e}");
                continue;
            }
        };

        let status = upstream.status();
        let headers = upstream.headers().clone();
        let class = adapter.classify_error(status, &headers).await;

        match class {
            ErrorClass::Success => {
                {
                    let mut st = state.runtime.entry(provider.id.clone()).or_default();
                    st.record_success();
                }
                match adapter
                    .transform_response(upstream, client_wanted_stream)
                    .await
                {
                    Ok(resp) => {
                        log(
                            &state,
                            &pool_id,
                            &provider.id,
                            Some(status.as_u16() as i64),
                            latency_ms,
                            true,
                        );
                        return resp;
                    }
                    Err(e) => {
                        // The upstream HTTP status was a success, but the
                        // body embedded an error (e.g. commandcode's
                        // ndjson error events) — log the real outcome, not
                        // the misleading raw status.
                        log(
                            &state,
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
                {
                    let mut st = state.runtime.entry(provider.id.clone()).or_default();
                    st.mark_misconfigured();
                }
                let content_type = headers.get(axum::http::header::CONTENT_TYPE).cloned();
                let text = upstream.text().await.unwrap_or_default();
                log(
                    &state,
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
                        let mut st = state.runtime.entry(provider.id.clone()).or_default();
                        st.mark_misconfigured();
                    }
                    let content_type = headers.get(axum::http::header::CONTENT_TYPE).cloned();
                    let text = upstream.text().await.unwrap_or_default();
                    log(
                        &state,
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
                let refreshed = with_refresh_lock(&state.refresh_locks, &provider.id, || async {
                    refresh_and_persist(&state, provider, adapter.as_ref(), &creds).await
                })
                .await;
                match refreshed {
                    Ok(new_creds) => {
                        // Retry the same provider once with new credentials.
                        let retry_req = match adapter.build_request(&body, &new_creds).await {
                            Ok(req) => req,
                            Err(e) => {
                                log(&state, &pool_id, &provider.id, None, 0, false);
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
                                        state.runtime.entry(provider.id.clone()).or_default();
                                    let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                                    st.record_retryable(cooldown, Instant::now());
                                }
                                log(&state, &pool_id, &provider.id, None, lat2, false);
                                last_error_body = format!("retry upstream request error: {e}");
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
                                        state.runtime.entry(provider.id.clone()).or_default();
                                    st.record_success();
                                }
                                match adapter
                                    .transform_response(resp2, client_wanted_stream)
                                    .await
                                {
                                    Ok(response) => {
                                        log(
                                            &state,
                                            &pool_id,
                                            &provider.id,
                                            Some(retry_status.as_u16() as i64),
                                            lat2,
                                            true,
                                        );
                                        return response;
                                    }
                                    Err(e) => {
                                        log(
                                            &state,
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
                                {
                                    let mut st =
                                        state.runtime.entry(provider.id.clone()).or_default();
                                    st.mark_misconfigured();
                                }
                                let text = resp2.text().await.unwrap_or_default();
                                log(
                                    &state,
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
                                        state.runtime.entry(provider.id.clone()).or_default();
                                    st.mark_misconfigured();
                                }
                                let text = resp2.text().await.unwrap_or_default();
                                log(
                                    &state,
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
                                            state.runtime.entry(provider.id.clone()).or_default();
                                        backoff::cooldown_for(st.backoff_level + 1)
                                    } else {
                                        Duration::from_secs(30)
                                    }
                                });
                                {
                                    let mut st =
                                        state.runtime.entry(provider.id.clone()).or_default();
                                    st.record_retryable(cooldown, Instant::now());
                                }
                                last_error_body = resp2.text().await.unwrap_or_default();
                                log(
                                    &state,
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
                            let mut st = state.runtime.entry(provider.id.clone()).or_default();
                            st.mark_misconfigured();
                        }
                        last_error_body = "refresh token invalid_grant; re-auth required".into();
                        log(&state, &pool_id, &provider.id, Some(401), latency_ms, false);
                        continue;
                    }
                    Err(RefreshError::Transient(msg)) => {
                        {
                            let mut st = state.runtime.entry(provider.id.clone()).or_default();
                            let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                            st.record_retryable(cooldown, Instant::now());
                        }
                        last_error_body = format!("transient refresh error: {msg}");
                        log(&state, &pool_id, &provider.id, Some(401), latency_ms, false);
                        continue;
                    }
                }
            }
            ErrorClass::Retryable { retry_after } => {
                let error_text = upstream.text().await.unwrap_or_default();

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
                            log(&state, &pool_id, &provider.id, None, 0, false);
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
                                    state.runtime.entry(provider.id.clone()).or_default();
                                let cooldown = backoff::cooldown_for(st.backoff_level + 1);
                                st.record_retryable(cooldown, Instant::now());
                            }
                            log(&state, &pool_id, &provider.id, None, lat2, false);
                            last_error_body = format!("retry upstream request error: {e}");
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
                                    state.runtime.entry(provider.id.clone()).or_default();
                                st.record_success();
                            }
                            match adapter
                                .transform_response(resp2, client_wanted_stream)
                                .await
                            {
                                Ok(response) => {
                                    log(
                                        &state,
                                        &pool_id,
                                        &provider.id,
                                        Some(retry_status.as_u16() as i64),
                                        lat2,
                                        true,
                                    );
                                    return response;
                                }
                                Err(e) => {
                                    log(
                                        &state,
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
                            {
                                let mut st =
                                    state.runtime.entry(provider.id.clone()).or_default();
                                st.mark_misconfigured();
                            }
                            last_error_body = resp2.text().await.unwrap_or_default();
                            log(
                                &state,
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
                                    state.runtime.entry(provider.id.clone()).or_default();
                                st.mark_misconfigured();
                            }
                            last_error_body = resp2.text().await.unwrap_or_default();
                            log(
                                &state,
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
                                    state.runtime.entry(provider.id.clone()).or_default();
                                backoff::cooldown_for(st.backoff_level + 1)
                            };
                            {
                                let mut st =
                                    state.runtime.entry(provider.id.clone()).or_default();
                                st.record_retryable(cooldown, Instant::now());
                            }
                            last_error_body = resp2.text().await.unwrap_or_default();
                            log(
                                &state,
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
                        let st = state.runtime.entry(provider.id.clone()).or_default();
                        backoff::cooldown_for(st.backoff_level + 1)
                    } else {
                        Duration::from_secs(30)
                    }
                });
                {
                    let mut st = state.runtime.entry(provider.id.clone()).or_default();
                    st.record_retryable(cooldown, Instant::now());
                }
                last_error_body = error_text;
                log(
                    &state,
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
        st.mark_misconfigured();
        assert!(!st.is_available(Instant::now()));
        assert!(matches!(st.status, ProviderStatus::Misconfigured));
    }
}
