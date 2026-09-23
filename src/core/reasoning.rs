//! Per-model reasoning-effort capability.
//!
//! Static lookup table (see `capability_for`) answering two questions about
//! one configured provider/model pairing:
//!
//! 1. does it accept a reasoning/thinking parameter at all, and
//! 2. if so, in which *wire shape* - OpenAI's `reasoning_effort: "<level>"`
//!    string, or Anthropic's `thinking: {"type":"enabled","budget_tokens":N}`
//!    object.
//!
//! It can be static (rather than the live per-request lookup the sibling
//! 9router gateway needs) because `Provider.upstream_model` is pinned once
//! at config time here.
//!
//! **Only three levels** (Low/Medium/High) are modelled. This is a product
//! choice to keep scope small, not an upstream limitation: Codex's Responses
//! API does currently accept `"xhigh"` on some models (e.g.
//! `gpt-5.1-codex-max`). We deliberately expose a narrower menu than
//! upstream supports rather than risk offering a level a given host's menu
//! lacks.

use serde::{Deserialize, Serialize};

use crate::core::model::{ProviderKind, WireFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningCapability {
    /// No reasoning parameter may be injected for this provider/model.
    Unsupported,
    /// OpenAI convention: a top-level `reasoning_effort: "low"|"medium"|"high"`
    /// string (Chat Completions) or `reasoning.effort` (Responses API).
    OpenAiEffort,
    /// Anthropic convention: a top-level
    /// `thinking: {"type":"enabled","budget_tokens": N}` object.
    AnthropicThinkingBudget,
}

impl ReasoningCapability {
    pub fn supports(&self) -> bool {
        !matches!(self, Self::Unsupported)
    }

    pub fn openai_effort_str(level: EffortLevel) -> &'static str {
        match level {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
        }
    }

    /// Hand-tuned budgets. Anthropic requires `budget_tokens >= 1024`, so
    /// `Low` sits at the floor rather than below it.
    pub fn anthropic_budget_tokens(level: EffortLevel) -> u32 {
        match level {
            EffortLevel::Low => 1024,
            EffortLevel::Medium => 8192,
            EffortLevel::High => 24576,
        }
    }
}

/// Dispatch is by `kind` FIRST, not by model-name pattern alone - the shape a
/// request must take is a property of which adapter builds it, not of the
/// model name. A `Passthrough` provider is never eligible for
/// `AnthropicThinkingBudget` unless its `wire_format` is actually
/// `Anthropic` (that's the literal wire shape `HttpAdapter` posts): a
/// Claude-named model served through an OpenAI-compatible mirror
/// (`wire_format: OpenAi`) must not get a `thinking` object injected into an
/// OpenAI-shaped body, that's a guaranteed 400.
///
/// **Documented day-one scope gaps** (explicit limitations, not bugs):
/// - legacy `claude-3-*` naming (`claude-3-5-sonnet-20241022`,
///   `claude-3-opus-20240229`, ...) is not matched by the
///   `claude-{sonnet,opus,haiku}-` prefix rule and stays `Unsupported`;
/// - OpenAI's `o1`/`o3`/`o4-mini` reasoning-capable models are not matched
///   by the `gpt-5` prefix rule and also stay `Unsupported`.
///
/// Both degrade to "no default injected", never to a broken request, so they
/// are acceptable until someone actually needs them.
pub fn capability_for(
    kind: ProviderKind,
    wire_format: WireFormat,
    upstream_model: &str,
) -> ReasoningCapability {
    let m = upstream_model.to_ascii_lowercase();
    match kind {
        // Matches today's unconditional `reasoning.effort = "medium"` default
        // in `codex::transform::transform_request`.
        ProviderKind::OauthCodex => ReasoningCapability::OpenAiEffort,
        ProviderKind::OauthCommandCode => {
            // Reuse the adapter's OWN predicate for which models get the
            // Anthropic-Messages-shaped request rather than restating it with
            // a separate, possibly-diverging pattern.
            if crate::providers::adapter::commandcode::transform::wants_messages_shape(&m) {
                ReasoningCapability::AnthropicThinkingBudget
            } else {
                ReasoningCapability::Unsupported
            }
        }
        ProviderKind::Passthrough => match wire_format {
            WireFormat::OpenAi if m.starts_with("gpt-5") => ReasoningCapability::OpenAiEffort,
            WireFormat::Anthropic
                if m.starts_with("claude-sonnet-")
                    || m.starts_with("claude-opus-")
                    || m.starts_with("claude-haiku-") =>
            {
                ReasoningCapability::AnthropicThinkingBudget
            }
            _ => ReasoningCapability::Unsupported,
        },
    }
}

/// Did the client's **original** request body already make an explicit
/// reasoning/thinking choice? Checked against the pre-translation body,
/// since wire-format translation can drop the very fields this looks for.
///
/// Three conventions, any one of which counts:
/// 1. top-level `reasoning_effort` (OpenAI Chat Completions - what a real
///    OpenAI SDK client sends)
/// 2. nested `reasoning.effort` (OpenAI Responses API)
/// 3. `thinking` (Anthropic Messages)
///
/// When any is present, no default is injected: the client wins, full stop.
pub fn client_specified_effort(client_json: &serde_json::Value) -> bool {
    if !client_json
        .get("reasoning_effort")
        .unwrap_or(&serde_json::Value::Null)
        .is_null()
    {
        return true;
    }
    if client_json
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .is_some_and(|e| !e.is_null())
    {
        return true;
    }
    !client_json
        .get("thinking")
        .unwrap_or(&serde_json::Value::Null)
        .is_null()
}

/// Set OpenAI's top-level `reasoning_effort` on an outgoing Chat
/// Completions body. (The Responses API's nested `reasoning.effort` shape
/// is handled by `codex::transform` instead.)
pub fn inject_openai_effort(body: &mut serde_json::Value, level: EffortLevel) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "reasoning_effort".into(),
            serde_json::Value::String(ReasoningCapability::openai_effort_str(level).into()),
        );
    }
}

/// Headroom kept between an injected `budget_tokens` and the request's
/// `max_tokens`: Anthropic rejects a request whose `max_tokens` isn't
/// strictly greater than `budget_tokens`, and a budget that leaves no room
/// for an actual answer is useless anyway.
const THINKING_MAX_TOKENS_HEADROOM: i64 = 1024;

/// Set Anthropic's `thinking` object on an outgoing Messages body **and
/// clean up what that makes invalid**:
///
/// - `max_tokens` must exceed `budget_tokens`; bump it to
///   `budget_tokens + 1024` when it's lower or absent (`claude_bridge`'s
///   translation defaults it to 4096, well under a `High` budget).
/// - `temperature` / `top_p` / `top_k` are rejected outright alongside
///   extended thinking; strip them rather than forwarding a request the
///   upstream is guaranteed to 400.
pub fn inject_anthropic_thinking(body: &mut serde_json::Value, level: EffortLevel) {
    let budget = ReasoningCapability::anthropic_budget_tokens(level) as i64;
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    obj.insert(
        "thinking".into(),
        serde_json::json!({ "type": "enabled", "budget_tokens": budget }),
    );
    let minimum = budget + THINKING_MAX_TOKENS_HEADROOM;
    let current = obj.get("max_tokens").and_then(serde_json::Value::as_i64);
    if current.is_none_or(|m| m < minimum) {
        obj.insert("max_tokens".into(), serde_json::json!(minimum));
    }
    for key in ["temperature", "top_p", "top_k"] {
        obj.remove(key);
    }
}

/// The whole "should I inject, and how" decision in one place, so every
/// adapter's request-time defense is literally the same code.
///
/// `resolved` is the effort already resolved by
/// `pools::select::resolve_reasoning_effort` (member override, else
/// provider default). Returns `Unsupported`-equivalent (`None`) - i.e.
/// "inject nothing" - when there's no configured effort, when the client
/// already asked for one, or when this provider/model can't carry one at
/// all. That last case is the request-time defense against a stale config
/// value introduced through a path that skipped write-time validation
/// (config import, the onboarding wizard, `<provider_id>/<model>` direct
/// addressing): skip silently, never fail the request.
pub fn effort_to_inject(
    kind: ProviderKind,
    wire_format: WireFormat,
    upstream_model: &str,
    resolved: Option<EffortLevel>,
    client_json: &serde_json::Value,
) -> Option<(ReasoningCapability, EffortLevel)> {
    let level = resolved?;
    if client_specified_effort(client_json) {
        tracing::debug!(
            model = upstream_model,
            "client set its own reasoning parameter; skipping default injection"
        );
        return None;
    }
    let capability = capability_for(kind, wire_format, upstream_model);
    if !capability.supports() {
        tracing::debug!(
            model = upstream_model,
            ?kind,
            ?wire_format,
            ?level,
            "configured reasoning effort is not supported by this provider/model; skipping injection"
        );
        return None;
    }
    Some((capability, level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_level_serializes_as_lowercase_text() {
        assert_eq!(serde_json::to_string(&EffortLevel::Low).unwrap(), "\"low\"");
        assert_eq!(
            serde_json::to_string(&EffortLevel::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&EffortLevel::High).unwrap(),
            "\"high\""
        );
        let l: EffortLevel = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(l, EffortLevel::High);
    }

    #[test]
    fn openai_effort_strings_match_the_serde_names() {
        assert_eq!(ReasoningCapability::openai_effort_str(EffortLevel::Low), "low");
        assert_eq!(
            ReasoningCapability::openai_effort_str(EffortLevel::Medium),
            "medium"
        );
        assert_eq!(
            ReasoningCapability::openai_effort_str(EffortLevel::High),
            "high"
        );
    }

    #[test]
    fn anthropic_budgets_are_monotonic_and_above_the_1024_floor() {
        let low = ReasoningCapability::anthropic_budget_tokens(EffortLevel::Low);
        let medium = ReasoningCapability::anthropic_budget_tokens(EffortLevel::Medium);
        let high = ReasoningCapability::anthropic_budget_tokens(EffortLevel::High);
        assert!(low >= 1024, "Anthropic rejects budget_tokens < 1024");
        assert!(low < medium && medium < high);
    }

    #[test]
    fn supports_is_false_only_for_unsupported() {
        assert!(!ReasoningCapability::Unsupported.supports());
        assert!(ReasoningCapability::OpenAiEffort.supports());
        assert!(ReasoningCapability::AnthropicThinkingBudget.supports());
    }

    #[test]
    fn codex_is_always_openai_effort_regardless_of_model_or_wire() {
        for model in ["gpt-5-codex", "gpt-4o", "anything-at-all"] {
            for wire in [WireFormat::OpenAi, WireFormat::Anthropic] {
                assert_eq!(
                    capability_for(ProviderKind::OauthCodex, wire, model),
                    ReasoningCapability::OpenAiEffort,
                    "{model} / {wire:?}"
                );
            }
        }
    }

    #[test]
    fn commandcode_delegates_to_wants_messages_shape() {
        // Claude-family -> /provider/v1/messages -> Anthropic thinking budget.
        assert_eq!(
            capability_for(
                ProviderKind::OauthCommandCode,
                WireFormat::OpenAi,
                "claude-sonnet-5"
            ),
            ReasoningCapability::AnthropicThinkingBudget
        );
        // Anything else goes to the OpenAI-shaped chat/completions endpoint,
        // which has no documented reasoning parameter here.
        assert_eq!(
            capability_for(
                ProviderKind::OauthCommandCode,
                WireFormat::OpenAi,
                "cc-model"
            ),
            ReasoningCapability::Unsupported
        );
        // The predicate really is the adapter's own - substring, not prefix.
        assert_eq!(
            capability_for(
                ProviderKind::OauthCommandCode,
                WireFormat::Anthropic,
                "anthropic/claude-opus-4"
            ),
            ReasoningCapability::AnthropicThinkingBudget
        );
    }

    #[test]
    fn passthrough_openai_wire_matches_the_gpt5_family() {
        assert_eq!(
            capability_for(ProviderKind::Passthrough, WireFormat::OpenAi, "gpt-5"),
            ReasoningCapability::OpenAiEffort
        );
        assert_eq!(
            capability_for(ProviderKind::Passthrough, WireFormat::OpenAi, "GPT-5.1-Codex"),
            ReasoningCapability::OpenAiEffort,
            "matching is case-insensitive"
        );
        assert_eq!(
            capability_for(ProviderKind::Passthrough, WireFormat::OpenAi, "gpt-4o"),
            ReasoningCapability::Unsupported
        );
    }

    #[test]
    fn passthrough_anthropic_wire_matches_the_modern_claude_families() {
        for model in [
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-1",
            "claude-haiku-4-5",
        ] {
            assert_eq!(
                capability_for(ProviderKind::Passthrough, WireFormat::Anthropic, model),
                ReasoningCapability::AnthropicThinkingBudget,
                "{model}"
            );
        }
    }

    #[test]
    fn wire_format_not_the_model_name_decides_the_shape() {
        // A Claude-named model served by an OpenAI-compatible mirror must
        // NOT get a `thinking` object injected into an OpenAI-shaped body.
        assert_eq!(
            capability_for(
                ProviderKind::Passthrough,
                WireFormat::OpenAi,
                "claude-sonnet-4-5"
            ),
            ReasoningCapability::Unsupported
        );
        // Symmetrically, a gpt-5 name behind an Anthropic-wire provider is
        // not eligible for `reasoning_effort` either.
        assert_eq!(
            capability_for(ProviderKind::Passthrough, WireFormat::Anthropic, "gpt-5"),
            ReasoningCapability::Unsupported
        );
    }

    #[test]
    fn documented_day_one_gap_legacy_claude_3_naming_is_unsupported() {
        for model in ["claude-3-5-sonnet-20241022", "claude-3-opus-20240229"] {
            assert_eq!(
                capability_for(ProviderKind::Passthrough, WireFormat::Anthropic, model),
                ReasoningCapability::Unsupported,
                "{model} is a documented day-one gap"
            );
        }
    }

    #[test]
    fn documented_day_one_gap_o_series_openai_models_are_unsupported() {
        for model in ["o1", "o3-mini", "o4-mini"] {
            assert_eq!(
                capability_for(ProviderKind::Passthrough, WireFormat::OpenAi, model),
                ReasoningCapability::Unsupported,
                "{model} is a documented day-one gap"
            );
        }
    }

    #[test]
    fn client_specified_effort_detects_all_three_conventions() {
        use serde_json::json;
        assert!(client_specified_effort(&json!({"reasoning_effort": "low"})));
        assert!(client_specified_effort(&json!({
            "reasoning": {"effort": "high"}
        })));
        assert!(client_specified_effort(&json!({
            "thinking": {"type": "enabled", "budget_tokens": 2048}
        })));
        assert!(client_specified_effort(&json!({
            "thinking": {"type": "disabled"}
        })));
    }

    #[test]
    fn client_specified_effort_is_false_for_absent_or_null_fields() {
        use serde_json::json;
        assert!(!client_specified_effort(&json!({"messages": []})));
        assert!(!client_specified_effort(&json!({
            "reasoning_effort": null, "thinking": null
        })));
        assert!(!client_specified_effort(&json!({ "reasoning": {} })));
    }

    #[test]
    fn inject_openai_effort_sets_the_top_level_string() {
        let mut body = serde_json::json!({"messages": []});
        inject_openai_effort(&mut body, EffortLevel::High);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn inject_anthropic_thinking_sets_the_object_and_normalizes() {
        let mut body = serde_json::json!({
            "messages": [], "max_tokens": 4096, "temperature": 0.7, "top_p": 0.9, "top_k": 5
        });
        inject_anthropic_thinking(&mut body, EffortLevel::High);
        let budget = ReasoningCapability::anthropic_budget_tokens(EffortLevel::High) as i64;
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], budget);
        assert_eq!(
            body["max_tokens"].as_i64().unwrap(),
            budget + 1024,
            "max_tokens must end up strictly above budget_tokens"
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
    }

    #[test]
    fn inject_anthropic_thinking_keeps_a_sufficiently_large_max_tokens() {
        let mut body = serde_json::json!({"messages": [], "max_tokens": 100_000});
        inject_anthropic_thinking(&mut body, EffortLevel::Low);
        assert_eq!(body["max_tokens"], 100_000);
    }

    #[test]
    fn inject_anthropic_thinking_supplies_max_tokens_when_absent() {
        let mut body = serde_json::json!({"messages": []});
        inject_anthropic_thinking(&mut body, EffortLevel::Medium);
        let budget = ReasoningCapability::anthropic_budget_tokens(EffortLevel::Medium) as i64;
        assert_eq!(body["max_tokens"].as_i64().unwrap(), budget + 1024);
    }

    #[test]
    fn effort_to_inject_returns_none_without_a_configured_effort() {
        assert!(effort_to_inject(
            ProviderKind::Passthrough,
            WireFormat::OpenAi,
            "gpt-5",
            None,
            &serde_json::json!({}),
        )
        .is_none());
    }

    #[test]
    fn effort_to_inject_yields_to_the_client() {
        for client in [
            serde_json::json!({"reasoning_effort": "low"}),
            serde_json::json!({"reasoning": {"effort": "low"}}),
            serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}),
        ] {
            assert!(
                effort_to_inject(
                    ProviderKind::Passthrough,
                    WireFormat::OpenAi,
                    "gpt-5",
                    Some(EffortLevel::High),
                    &client,
                )
                .is_none(),
                "{client}"
            );
        }
    }

    #[test]
    fn effort_to_inject_silently_skips_a_stale_unsupported_config() {
        // The request-time defense: a value that reached this point through
        // config import / the onboarding wizard / direct addressing must
        // degrade to "inject nothing", never to an error.
        assert!(effort_to_inject(
            ProviderKind::Passthrough,
            WireFormat::OpenAi,
            "gpt-4o",
            Some(EffortLevel::High),
            &serde_json::json!({}),
        )
        .is_none());
    }

    #[test]
    fn effort_to_inject_reports_the_capability_and_level_when_it_applies() {
        assert_eq!(
            effort_to_inject(
                ProviderKind::Passthrough,
                WireFormat::OpenAi,
                "gpt-5.1",
                Some(EffortLevel::Medium),
                &serde_json::json!({"messages": []}),
            ),
            Some((ReasoningCapability::OpenAiEffort, EffortLevel::Medium))
        );
        assert_eq!(
            effort_to_inject(
                ProviderKind::Passthrough,
                WireFormat::Anthropic,
                "claude-sonnet-4-5",
                Some(EffortLevel::Low),
                &serde_json::json!({"messages": []}),
            ),
            Some((
                ReasoningCapability::AnthropicThinkingBudget,
                EffortLevel::Low
            ))
        );
    }
}
