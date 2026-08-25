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
}
