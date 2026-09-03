use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum WireFormat {
    OpenAi,
    Anthropic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum ProviderKind {
    Passthrough,
    OauthCodex,
    OauthCommandCode,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub wire_format: WireFormat,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub upstream_model: String,
    /// Opt-in: capture raw request/response bytes for every successful
    /// exchange served by this provider, as JSONL (`telemetry::dataset_log`).
    /// The base setting — also the only one consulted for
    /// `<provider_id>/<model>` direct addressing, which has no `PoolMember`
    /// row to override it. `#[serde(default)]` because `Provider` is
    /// deserialized directly by config export/import and seed loading,
    /// neither of which has this key in a pre-existing file. See
    /// docs/superpowers/specs/2026-08-27-dataset-logging-design.md.
    #[serde(default)]
    pub dataset_logging: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Selection strategy for a pool's members. `Priority` is the original,
/// default behavior: `select()` always sorts members by `priority`
/// ascending and returns that fixed order (the caller's failover loop then
/// tries them front-to-back). `RoundRobin` rotates the head of that
/// priority-sorted list on each selection (see `pools::select`), so normal
/// traffic spreads across members instead of always hitting the lowest
/// priority one first - failover still falls through the rest of the
/// rotated list in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum PoolStrategy {
    #[default]
    Priority,
    RoundRobin,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Pool {
    pub id: String,
    pub wire_format: WireFormat,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub strategy: PoolStrategy,
    /// Requests to keep sending to the same rotated-in member before
    /// advancing to the next one. Only meaningful when `strategy` is
    /// `RoundRobin`; `None` (or any non-positive value) normalizes to `1`
    /// (rotate every selection) - see `pools::select::rotate_from_cursor`.
    pub sticky_limit: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct PoolMember {
    pub pool_id: String,
    pub provider_id: String,
    pub priority: i64,
    /// Overrides the provider's `upstream_model` for requests routed through
    /// this pool/provider pairing. Lets one provider (one credential set) be
    /// shared across several pools that each target a different upstream
    /// model.
    pub model_override: Option<String>,
    /// Overrides `Provider.dataset_logging` for requests routed through
    /// this specific pool membership. `None` inherits the provider's own
    /// setting (`pools::select::dataset_logging_enabled`). `#[serde(default)]`
    /// for the same reason as `Provider.dataset_logging` — see its doc
    /// comment.
    #[serde(default)]
    pub dataset_logging_override: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolWithMembers {
    pub pool: Pool,
    pub members: Vec<PoolMember>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct OAuthState {
    pub provider_id: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
    #[sqlx(json)]
    pub provider_data: serde_json::Value,
    pub pkce_verifier: Option<String>,
    pub oauth_state: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub pool_id: Option<String>,
    pub provider_id: Option<String>,
    pub status_code: Option<i64>,
    pub latency_ms: i64,
    pub success: bool,
}

/// Time-to-first-byte and total wall-clock duration for one dataset-logged
/// exchange. A nested struct (not two flat fields on `DatasetLogEntry`) so
/// it serializes as the nested `"latency_ms": {"ttfb_ms": ..., "total_ms":
/// ...}` on-disk shape the design spec commits to. `ttfb_ms` is the
/// existing duration already computed around `state.http.execute` in
/// `handle_proxy`; `total_ms` requires a separate timer read when the
/// response stream actually ends (see `proxy::dataset_tee`), since it
/// isn't derivable from the upstream-request timer alone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyMs {
    pub ttfb_ms: Option<i64>,
    pub total_ms: i64,
}

/// One JSONL record written by `telemetry::dataset_log` for a successful
/// proxy exchange whose pool/provider opted into dataset logging. See
/// docs/superpowers/specs/2026-08-27-dataset-logging-design.md for the
/// full field-by-field rationale.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatasetLogEntry {
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    /// `None` for a direct-provider-addressed call (no `PoolMember` row).
    pub pool_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    /// Reserved for a future "User credential" feature - always `None`
    /// until that exists.
    pub user_id: Option<String>,
    pub wire_format: WireFormat,
    pub stream: bool,
    pub input_body: String,
    pub output_body: String,
    /// `false` when the response ended before finishing cleanly (client
    /// disconnect, or an upstream error mid-stream) - `output_body` then
    /// holds only whatever bytes were accumulated up to that point.
    /// Curation should discard `complete: false` records by default.
    pub complete: bool,
    pub latency_ms: LatencyMs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_serializes_as_lowercase_text() {
        assert_eq!(
            serde_json::to_string(&WireFormat::OpenAi).unwrap(),
            "\"openai\""
        );
        assert_eq!(
            serde_json::to_string(&WireFormat::Anthropic).unwrap(),
            "\"anthropic\""
        );
        let w: WireFormat = serde_json::from_str("\"anthropic\"").unwrap();
        assert!(matches!(w, WireFormat::Anthropic));
    }

    #[test]
    fn provider_kind_serializes_with_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::OauthCodex).unwrap(),
            "\"oauth_codex\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::OauthCommandCode).unwrap(),
            "\"oauth_command_code\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::Passthrough).unwrap(),
            "\"passthrough\""
        );
        let command_code: ProviderKind = serde_json::from_str("\"oauth_command_code\"").unwrap();
        assert_eq!(command_code, ProviderKind::OauthCommandCode);
    }

    #[test]
    fn pool_strategy_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&PoolStrategy::Priority).unwrap(),
            "\"priority\""
        );
        assert_eq!(
            serde_json::to_string(&PoolStrategy::RoundRobin).unwrap(),
            "\"round_robin\""
        );
        let s: PoolStrategy = serde_json::from_str("\"round_robin\"").unwrap();
        assert_eq!(s, PoolStrategy::RoundRobin);
    }

    #[test]
    fn pool_strategy_defaults_to_priority() {
        assert_eq!(PoolStrategy::default(), PoolStrategy::Priority);
    }

    #[test]
    fn provider_and_pool_member_carry_dataset_logging_fields() {
        let p = Provider {
            id: "p1".into(),
            name: "P1".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough,
            base_url: Some("u".into()),
            api_key: Some("k".into()),
            upstream_model: "m".into(),
            dataset_logging: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(p.dataset_logging);

        let m = PoolMember {
            pool_id: "pool1".into(),
            provider_id: "p1".into(),
            priority: 1,
            model_override: None,
            dataset_logging_override: Some(false),
        };
        assert_eq!(m.dataset_logging_override, Some(false));
    }
}
