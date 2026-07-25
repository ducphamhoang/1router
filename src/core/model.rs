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

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Pool {
    pub id: String,
    pub wire_format: WireFormat,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct PoolMember {
    pub pool_id: String,
    pub provider_id: String,
    pub priority: i64,
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
        assert_eq!(serde_json::to_string(&WireFormat::OpenAi).unwrap(), "\"openai\"");
        assert_eq!(serde_json::to_string(&WireFormat::Anthropic).unwrap(), "\"anthropic\"");
        let w: WireFormat = serde_json::from_str("\"anthropic\"").unwrap();
        assert!(matches!(w, WireFormat::Anthropic));
    }

    #[test]
    fn provider_kind_serializes_with_snake_case() {
        assert_eq!(serde_json::to_string(&ProviderKind::OauthCodex).unwrap(), "\"oauth_codex\"");
        assert_eq!(serde_json::to_string(&ProviderKind::Passthrough).unwrap(), "\"passthrough\"");
    }
}
