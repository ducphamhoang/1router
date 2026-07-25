use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

pub fn init_tracing() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json().with_current_span(true))
            .init();
    });
}

pub fn redact(secret: &str, text: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_masks_secret_occurrences() {
        let out = redact("sk-abc123", "authorization: Bearer sk-abc123 done");
        assert!(!out.contains("sk-abc123"));
        assert!(out.contains("***"));
    }

    #[test]
    fn redact_empty_secret_is_noop() {
        assert_eq!(redact("", "hello"), "hello");
    }

    #[test]
    fn init_tracing_is_idempotent() {
        init_tracing();
        init_tracing(); // must not panic on second call
    }
}
