use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};

use crate::core::error::ErrorClass;

pub const MAX_BACKOFF_LEVEL: u8 = 15;

pub fn classify(status: StatusCode, headers: &HeaderMap) -> ErrorClass {
    if status.is_success() {
        return ErrorClass::Success;
    }

    match status {
        StatusCode::UNAUTHORIZED => ErrorClass::AuthExpired,
        StatusCode::BAD_REQUEST => ErrorClass::NonRetryable,
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::REQUEST_TIMEOUT
        | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => ErrorClass::Retryable {
            retry_after: reset_after_from_header(headers),
        },
        s if s.is_server_error() => ErrorClass::Retryable {
            retry_after: reset_after_from_header(headers),
        },
        _ => ErrorClass::Retryable { retry_after: None },
    }
}

pub fn cooldown_for(level: u8) -> Duration {
    let level = level.max(1);
    let exp = (level - 1).min(MAX_BACKOFF_LEVEL) as u32;
    let secs = 2u64.saturating_mul(2u64.saturating_pow(exp));
    Duration::from_secs(secs.min(300))
}

pub fn reset_after_from_header(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get("retry-after")?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(1800)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::ErrorClass;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use std::time::Duration;

    #[test]
    fn cooldown_formula_and_cap() {
        assert_eq!(cooldown_for(1), Duration::from_secs(2)); // 2s * 2^0
        assert_eq!(cooldown_for(2), Duration::from_secs(4)); // 2s * 2^1
        assert_eq!(cooldown_for(3), Duration::from_secs(8));
        // capped at 5 minutes
        assert_eq!(cooldown_for(15), Duration::from_secs(300));
        assert_eq!(cooldown_for(99), Duration::from_secs(300));
    }

    #[test]
    fn classify_success_and_client_errors() {
        let h = HeaderMap::new();
        assert_eq!(classify(StatusCode::OK, &h), ErrorClass::Success);
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, &h),
            ErrorClass::AuthExpired
        );
        assert_eq!(
            classify(StatusCode::BAD_REQUEST, &h),
            ErrorClass::NonRetryable
        );
    }

    #[test]
    fn classify_retryable() {
        let h = HeaderMap::new();
        assert!(matches!(
            classify(StatusCode::TOO_MANY_REQUESTS, &h),
            ErrorClass::Retryable { .. }
        ));
        assert!(matches!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, &h),
            ErrorClass::Retryable { .. }
        ));
        assert!(matches!(
            classify(StatusCode::REQUEST_TIMEOUT, &h),
            ErrorClass::Retryable { .. }
        ));
    }

    #[test]
    fn retry_after_header_seconds_is_parsed_and_capped() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("120"));
        assert_eq!(reset_after_from_header(&h), Some(Duration::from_secs(120)));

        h.insert("retry-after", HeaderValue::from_static("999999"));
        assert_eq!(reset_after_from_header(&h), Some(Duration::from_secs(1800)));
        // cap 30min
    }
}
