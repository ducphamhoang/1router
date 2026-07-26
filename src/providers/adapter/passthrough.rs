use std::net::IpAddr;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::core::error::{AppError, ErrorClass, RefreshError};
use crate::core::model::{Provider, WireFormat};
use crate::providers::adapter::{Credentials, ProviderAdapter};
use crate::proxy::backoff;

const ANTHROPIC_VERSION: &str = "2023-06-01";

fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(&['[', ']'][..]);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || matches!(ip.segments()[0] & 0xfe00, 0xfc00)
                || matches!(ip.segments()[0] & 0xffc0, 0xfe80)
        }
        Err(_) => false,
    }
}

fn validate_upstream_url(
    raw: &str,
    allow_insecure_upstreams: bool,
) -> Result<reqwest::Url, AppError> {
    let url =
        reqwest::Url::parse(raw).map_err(|_| AppError::Internal("invalid upstream URL".into()))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_insecure_upstreams => {}
        _ => {
            return Err(AppError::Internal(
                "upstream URL must use https unless explicitly allowed".into(),
            ));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| AppError::Internal("upstream URL missing host".into()))?;
    if !allow_insecure_upstreams && is_private_host(host) {
        return Err(AppError::Internal(
            "private-network upstream URL blocked".into(),
        ));
    }
    Ok(url)
}

pub struct PassthroughAdapter {
    provider: Provider,
    http: reqwest::Client,
    allow_insecure_upstreams: bool,
}

impl PassthroughAdapter {
    pub fn new(provider: Provider, http: reqwest::Client, allow_insecure_upstreams: bool) -> Self {
        Self {
            provider,
            http,
            allow_insecure_upstreams,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for PassthroughAdapter {
    async fn build_request(
        &self,
        client_body: &Bytes,
        creds: &Credentials,
    ) -> Result<reqwest::Request, AppError> {
        let mut json: serde_json::Value = serde_json::from_slice(client_body)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;
        if let Some(obj) = json.as_object_mut() {
            obj.insert(
                "model".into(),
                serde_json::Value::String(self.provider.upstream_model.clone()),
            );
        }
        let raw_url =
            self.provider.base_url.clone().ok_or_else(|| {
                AppError::Internal("passthrough provider missing base_url".into())
            })?;
        let url = validate_upstream_url(&raw_url, self.allow_insecure_upstreams)?;

        let mut builder = self.http.post(url).json(&json);
        if let Some(key) = creds.api_key.as_ref() {
            builder = match self.provider.wire_format {
                WireFormat::OpenAi => builder.bearer_auth(key),
                WireFormat::Anthropic => builder
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION),
            };
        }
        builder
            .build()
            .map_err(|e| AppError::Internal(format!("request build failed: {e}")))
    }

    async fn transform_response(
        &self,
        upstream: reqwest::Response,
        _client_wanted_stream: bool,
    ) -> Result<Response, AppError> {
        let status = upstream.status();
        let mut resp_headers = HeaderMap::new();
        for (k, v) in upstream.headers().iter() {
            if k.as_str().eq_ignore_ascii_case("transfer-encoding") {
                continue;
            }
            resp_headers.insert(k.clone(), v.clone());
        }
        let stream = upstream.bytes_stream();
        let body = Body::from_stream(stream);
        let mut response = (status, body).into_response();
        *response.headers_mut() = resp_headers;
        Ok(response)
    }

    async fn classify_error(&self, status: StatusCode, headers: &HeaderMap) -> ErrorClass {
        backoff::classify(status, headers)
    }

    fn needs_refresh(&self, _creds: &Credentials) -> bool {
        false
    }

    async fn refresh_credentials(&self, _creds: &Credentials) -> Result<Credentials, RefreshError> {
        Err(RefreshError::Transient("passthrough has no refresh".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use crate::providers::adapter::Credentials;
    use bytes::Bytes;
    use chrono::Utc;

    fn prov() -> Provider {
        Provider {
            id: "p1".into(),
            name: "P1".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough,
            base_url: Some("https://api.example.com/v1/chat/completions".into()),
            api_key: Some("sk-xyz".into()),
            upstream_model: "real-model".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn creds() -> Credentials {
        Credentials {
            api_key: Some("sk-xyz".into()),
            access_token: None,
            refresh_token: None,
            id_token: None,
            access_expires_at: None,
            provider_data: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn build_request_rewrites_model_and_sets_auth() {
        let a = PassthroughAdapter::new(prov(), reqwest::Client::new(), true);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o", "messages": []
            }))
            .unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert_eq!(
            req.headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer sk-xyz"
        );
        let sent: serde_json::Value =
            serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(sent["model"], "real-model");
    }

    #[tokio::test]
    async fn build_request_uses_anthropic_headers_for_anthropic_wire_format() {
        let mut p = prov();
        p.wire_format = WireFormat::Anthropic;
        let a = PassthroughAdapter::new(p, reqwest::Client::new(), true);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({ "model": "claude", "messages": [] })).unwrap(),
        );
        let req = a.build_request(&body, &creds()).await.unwrap();

        assert!(req.headers().get("authorization").is_none());
        assert_eq!(
            req.headers().get("x-api-key").unwrap().to_str().unwrap(),
            "sk-xyz"
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .unwrap()
                .to_str()
                .unwrap(),
            ANTHROPIC_VERSION
        );
    }

    #[tokio::test]
    async fn blocks_private_upstream_by_default() {
        let mut p = prov();
        p.base_url = Some("http://127.0.0.1:8080/v1/chat/completions".into());
        let a = PassthroughAdapter::new(p, reqwest::Client::new(), false);
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o", "messages": []
            }))
            .unwrap(),
        );
        let err = a.build_request(&body, &creds()).await.unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn needs_refresh_is_false() {
        let a = PassthroughAdapter::new(prov(), reqwest::Client::new(), true);
        assert!(!a.needs_refresh(&creds()));
    }
}
