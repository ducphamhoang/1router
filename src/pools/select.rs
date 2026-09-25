use crate::core::model::{
    EffortLevel, Modality, Pool, PoolMember, PoolStrategy, Provider, ProviderKind, WireFormat,
};
use crate::core::state::{ConfigSnapshot, DiscoveredModelsMap, PoolRotationMap};

/// One resolved routing candidate: a provider plus everything the
/// membership it was reached through says about how to call it.
///
/// Was a bare `(&Provider, String, Option<bool>)` tuple until a fourth
/// member-level override arrived and destructuring stopped being readable.
pub struct ResolvedMember<'a> {
    pub provider: &'a Provider,
    /// The member's `model_override` if set, else the provider's own
    /// `upstream_model` - this is what lets one provider (one credential
    /// set) be shared across pools that each call a different model.
    pub effective_model: String,
    /// `PoolMember.dataset_logging_override` for a pool-routed entry, or
    /// `None` for a direct-provider-addressed one (which has no
    /// `PoolMember` row at all) - either way, pass it to
    /// `dataset_logging_enabled` alongside the provider to resolve the
    /// effective setting.
    pub dataset_logging_override: Option<bool>,
    /// `PoolMember.reasoning_effort_override`, same `None`-means-inherit
    /// contract as `dataset_logging_override` - resolve it with
    /// `resolve_reasoning_effort`.
    pub reasoning_effort_override: Option<EffortLevel>,
}

pub struct Selection<'a> {
    /// `None` for a direct `<provider_id>/<model>` selection (see
    /// `select_direct_provider` below) - there's no real pool row behind it.
    pub pool: Option<&'a Pool>,
    /// Candidates in priority order (the caller's failover loop tries them
    /// front-to-back).
    pub providers: Vec<ResolvedMember<'a>>,
}

/// `member_override` is `PoolMember.dataset_logging_override` for a
/// pool-routed call, or `None` for direct-provider addressing (which has
/// no `PoolMember` row at all) - either way, `None` means "inherit the
/// provider's own setting".
pub fn dataset_logging_enabled(provider: &Provider, member_override: Option<bool>) -> bool {
    member_override.unwrap_or(provider.dataset_logging)
}

/// Same `None`-means-inherit contract as `dataset_logging_enabled`: a
/// pool member's own `reasoning_effort_override` wins over the provider's
/// `default_reasoning_effort`; `None` on both means "inject nothing".
///
/// Whether the resolved level can actually be sent is a separate question
/// answered by `core::reasoning::capability_for` at request-build time in
/// each adapter - this function only resolves precedence.
pub fn resolve_reasoning_effort(
    provider: &Provider,
    member_override: Option<EffortLevel>,
) -> Option<EffortLevel> {
    member_override.or(provider.default_reasoning_effort)
}

/// Resolve a client-requested `model` to what to actually call.
///
/// Resolution is a two-step process:
///
/// 1. Look up `model` as a real pool id. If a pool with that id exists,
///    use it only if `pool.wire_format ==` the requested wire format;
///    otherwise return `None`. There is **no** fallback to step 2 in that
///    case.
///
/// 2. If no real pool matches by id, fall back to `<provider_id>/<model>`
///    direct addressing in `select_direct_provider`, which splits on the
///    first `/` via `str::split_once('/')`.
///
/// Direct addressing exists so that a provider offering several models
/// (e.g. DeepSeek's `deepseek-v4-flash`/`deepseek-v4-pro`) doesn't need one
/// throwaway 1-member pool per model just to make each one callable; it's a
/// single specific provider, so there's no failover across it, unlike a real
/// pool. The split is unambiguous: pool ids and provider ids can never
/// contain `/` (enforced by `validate_path_id` at creation), so this syntax
/// can never collide with a real pool id.
pub fn select<'a>(
    snapshot: &'a ConfigSnapshot,
    pool_id: &str,
    wire: WireFormat,
    rotation: &PoolRotationMap,
) -> Option<Selection<'a>> {
    if let Some(pwm) = snapshot.pools.iter().find(|p| p.pool.id == pool_id) {
        // Checked before `rotate_from_cursor`, so a rejected request never
        // advances the pool's rotation cursor.
        if pwm.pool.wire_format != wire || pwm.pool.modality != Modality::Chat {
            return None;
        }

        let mut members = pwm.members.clone();
        members.sort_by_key(|m| m.priority);

        if pwm.pool.strategy == PoolStrategy::RoundRobin && members.len() > 1 {
            members = rotate_from_cursor(&pwm.pool, members, rotation);
        }

        let providers = members
            .iter()
            .filter_map(|m| {
                let provider = snapshot.providers.iter().find(|p| p.id == m.provider_id)?;
                let model = m
                    .model_override
                    .clone()
                    .unwrap_or_else(|| provider.upstream_model.clone());
                Some(ResolvedMember {
                    provider,
                    effective_model: model,
                    dataset_logging_override: m.dataset_logging_override,
                    reasoning_effort_override: m.reasoning_effort_override,
                })
            })
            .collect();

        return Some(Selection {
            pool: Some(&pwm.pool),
            providers,
        });
    }

    select_direct_provider(snapshot, pool_id, wire)
}

/// Image models a Codex provider can serve through the `image_generation`
/// tool: `gpt-image-*` (hosted by `MediaConfig::codex_image_host_model`) or
/// `<chat-model>-image` (hosted by `<chat-model>` itself).
pub fn is_image_model(model: &str) -> bool {
    let valid = |s: &str| !s.is_empty() && s.len() <= MAX_DIRECT_MODEL_LEN
        && !s.chars().any(|c| c.is_control() || c.is_whitespace());
    valid(model)
        && (model.strip_prefix("gpt-image-").is_some_and(|rest| !rest.is_empty())
            || model.strip_suffix("-image").is_some_and(|host| !host.is_empty()))
}

/// Whether a member may serve an image pool: Codex providers with an image
/// `model_override` only. Enforced on write by `queries::upsert_member`,
/// re-checked here because config import bypasses that.
fn image_member_eligible(provider: &Provider, member: &PoolMember) -> bool {
    provider.kind == ProviderKind::OauthCodex
        && member.model_override.as_deref().is_some_and(is_image_model)
}

/// `select`'s counterpart for `/v1/images/generations`: `model` is an
/// `Image`-modality pool id (a chat pool yields `None`, checked before
/// rotation), or `<codex_provider_id>/<image_model>` direct addressing -
/// which the caller must restrict to the admin (`Selection.pool == None`).
/// `wire_format` is ignored: image pools speak the OpenAI Images API.
pub fn select_image<'a>(
    snapshot: &'a ConfigSnapshot,
    pool_id: &str,
    rotation: &PoolRotationMap,
) -> Option<Selection<'a>> {
    if let Some(pwm) = snapshot.pools.iter().find(|p| p.pool.id == pool_id) {
        if pwm.pool.modality != Modality::Image {
            return None;
        }
        let mut members: Vec<PoolMember> = pwm
            .members
            .iter()
            .filter(|m| {
                snapshot
                    .providers
                    .iter()
                    .find(|p| p.id == m.provider_id)
                    .is_some_and(|p| image_member_eligible(p, m))
            })
            .cloned()
            .collect();
        members.sort_by_key(|m| m.priority);
        if pwm.pool.strategy == PoolStrategy::RoundRobin && members.len() > 1 {
            members = rotate_from_cursor(&pwm.pool, members, rotation);
        }
        let providers = members
            .iter()
            .filter_map(|m| {
                let provider = snapshot.providers.iter().find(|p| p.id == m.provider_id)?;
                Some(ResolvedMember {
                    provider,
                    effective_model: m.model_override.clone()?,
                    dataset_logging_override: Some(false),
                    reasoning_effort_override: None,
                })
            })
            .collect();
        return Some(Selection {
            pool: Some(&pwm.pool),
            providers,
        });
    }

    let (provider_id, model) = pool_id.split_once('/')?;
    if !is_image_model(model) {
        return None;
    }
    let provider = snapshot
        .providers
        .iter()
        .find(|p| p.id == provider_id && p.kind == ProviderKind::OauthCodex)?;
    Some(Selection {
        pool: None,
        providers: vec![ResolvedMember {
            provider,
            effective_model: model.to_string(),
            dataset_logging_override: Some(false),
            reasoning_effort_override: None,
        }],
    })
}

/// Rotate `members` (already priority-sorted) so the pool's rotation cursor
/// becomes the head, then advance the cursor - every `sticky_limit`
/// selections, not every one, so a strategy switch doesn't thrash a
/// provider connection on every single request.
///
/// Only the *head* changes; the rest of the list stays in the same
/// relative (priority) order behind it, so the caller's failover loop
/// (`proxy::flow`) still has a well-defined fallback tail if the rotated-in
/// member fails - rotation and failover are the same ordered `Vec`, not two
/// competing mechanisms.
///
/// The cursor is read modulo `members.len()`, so a member removed since the
/// cursor last advanced can never leave it out of range (mirrors 9router's
/// `combo.js`: `currentIndex = state.index % models.length`).
fn rotate_from_cursor(
    pool: &Pool,
    mut members: Vec<PoolMember>,
    rotation: &PoolRotationMap,
) -> Vec<PoolMember> {
    let len = members.len();
    let sticky_limit = normalize_sticky_limit(pool.sticky_limit);

    let mut state = rotation.entry(pool.id.clone()).or_default();
    let head = state.index % len;
    members.rotate_left(head);

    if state.consecutive_uses + 1 >= sticky_limit {
        state.index = (head + 1) % len;
        state.consecutive_uses = 0;
    } else {
        state.index = head;
        state.consecutive_uses += 1;
    }

    members
}

/// Any non-positive or absent sticky limit normalizes to `1` (rotate every
/// selection) - mirrors 9router's `combo.js::normalizeStickyLimit`.
fn normalize_sticky_limit(sticky_limit: Option<i64>) -> u32 {
    match sticky_limit {
        Some(n) if n > 0 => n as u32,
        _ => 1,
    }
}

/// Longest `<model>` accepted in `<provider_id>/<model>` direct addressing.
/// Real model ids are far shorter; the cap keeps a caller-chosen string from
/// becoming an arbitrarily large runtime-state key (SEC-03).
pub const MAX_DIRECT_MODEL_LEN: usize = 200;

fn select_direct_provider<'a>(
    snapshot: &'a ConfigSnapshot,
    requested: &str,
    _wire: WireFormat,
) -> Option<Selection<'a>> {
    let (provider_id, model) = requested.split_once('/')?;
    if model.is_empty()
        || model.len() > MAX_DIRECT_MODEL_LEN
        || model.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let provider = snapshot.providers.iter().find(|p| p.id == provider_id)?;
    Some(Selection {
        pool: None,
        providers: vec![ResolvedMember {
            provider,
            effective_model: model.to_string(),
            dataset_logging_override: None,
            reasoning_effort_override: None,
        }],
    })
}

/// Whether a non-admin caller may use `<provider_id>/<model>` direct
/// addressing for `model` (SEC-06). Allowed: the provider's own default
/// model, a model some pool member already routes to on this provider, or a
/// model the provider's live `/models` listing reported (what
/// `GET /v1/models` advertises). Anything else would let any caller bill an
/// arbitrary model to the admin's key, bypassing the pools the admin set up.
/// The shared-secret admin is not restricted. Only chat pools count: an
/// image pool's `gpt-image-*` override must not become chat-addressable.
pub fn direct_model_allowed(
    snapshot: &ConfigSnapshot,
    discovered: &DiscoveredModelsMap,
    provider: &Provider,
    model: &str,
) -> bool {
    provider.upstream_model == model
        || snapshot.pools.iter().filter(|p| p.pool.modality == Modality::Chat).any(|p| {
            p.members
                .iter()
                .any(|m| m.provider_id == provider.id && m.model_override.as_deref() == Some(model))
        })
        || discovered
            .get(&provider.id)
            .is_some_and(|models| models.iter().any(|m| m == model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Pool, PoolMember, PoolWithMembers, Provider, ProviderKind, WireFormat};
    use crate::core::state::{ConfigSnapshot, DiscoveredModelsMap, PoolRotationMap};
    use chrono::Utc;
    use std::sync::Arc;

    fn prov(id: &str) -> Provider {
        Provider {
            id: id.into(), name: id.into(), wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough, base_url: Some("u".into()),
            api_key: Some("k".into()), upstream_model: "m".into(),
            dataset_logging: false,
            default_reasoning_effort: None,
            created_at: Utc::now(), updated_at: Utc::now(),
        }
    }

    fn empty_rotation() -> PoolRotationMap {
        Arc::new(dashmap::DashMap::new())
    }

    fn snap() -> ConfigSnapshot {
        snap_with_strategy(PoolStrategy::Priority, None)
    }

    fn snap_with_strategy(strategy: PoolStrategy, sticky_limit: Option<i64>) -> ConfigSnapshot {
        ConfigSnapshot {
            providers: vec![prov("a"), prov("b")],
            pools: vec![PoolWithMembers {
                pool: Pool {
                    id: "gpt-4o".into(), wire_format: WireFormat::OpenAi, created_at: Utc::now(),
                    strategy, sticky_limit, modality: Modality::Chat,
                },
                members: vec![
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "b".into(), priority: 20, model_override: None, dataset_logging_override: None, reasoning_effort_override: None },
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "a".into(), priority: 10, model_override: None, dataset_logging_override: None, reasoning_effort_override: None },
                ],
            }],
        }
    }

    #[test]
    fn orders_by_priority_ascending() {
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let ids: Vec<&str> = sel.providers.iter().map(|m| m.provider.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn model_override_replaces_provider_upstream_model() {
        let mut s = snap();
        s.pools[0].members.push(PoolMember {
            pool_id: "gpt-4o".into(),
            provider_id: "a".into(),
            priority: 10,
            model_override: Some("gpt-5.6-sol".into()),
            dataset_logging_override: None,
            reasoning_effort_override: None,
        });
        // dedupe: replace the "a" member from `snap()` with the overridden one
        s.pools[0].members.retain(|m| m.provider_id != "a" || m.model_override.is_some());

        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let a = sel.providers.iter().find(|m| m.provider.id == "a").unwrap();
        assert_eq!(a.effective_model, "gpt-5.6-sol");

        let b = sel.providers.iter().find(|m| m.provider.id == "b").unwrap();
        assert_eq!(b.effective_model, "m", "falls back to the provider's own upstream_model when unset");
    }

    #[test]
    fn wrong_wire_format_returns_none() {
        assert!(select(&snap(), "gpt-4o", WireFormat::Anthropic, &empty_rotation()).is_none());
    }

    #[test]
    fn missing_pool_returns_none() {
        assert!(select(&snap(), "nope", WireFormat::OpenAi, &empty_rotation()).is_none());
    }

    #[test]
    fn direct_provider_slash_model_routes_to_that_provider_with_that_model() {
        let s = snap();
        let sel = select(&s, "a/some-other-model", WireFormat::OpenAi, &empty_rotation()).unwrap();
        assert!(sel.pool.is_none());
        assert_eq!(sel.providers.len(), 1);
        assert_eq!(sel.providers[0].provider.id, "a");
        assert_eq!(sel.providers[0].effective_model, "some-other-model");
    }

    #[test]
    fn direct_provider_addressing_only_splits_on_the_first_slash() {
        let s = snap();
        let sel = select(&s, "a/meta-llama/Llama-3-70b", WireFormat::OpenAi, &empty_rotation()).unwrap();
        assert_eq!(sel.providers[0].provider.id, "a");
        assert_eq!(sel.providers[0].effective_model, "meta-llama/Llama-3-70b");
    }

    #[test]
    fn direct_provider_addressing_is_only_a_fallback_a_real_pool_still_wins() {
        // "gpt-4o" is a real pool with no '/' - direct addressing never
        // applies here regardless.
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        assert!(sel.pool.is_some());
    }

    #[test]
    fn direct_provider_addressing_rejects_oversized_or_malformed_models() {
        let s = snap();
        let long = format!("a/{}", "x".repeat(MAX_DIRECT_MODEL_LEN + 1));
        for bad in [long.as_str(), "a/", "a/has space", "a/ctl\u{1}"] {
            assert!(select(&s, bad, WireFormat::OpenAi, &empty_rotation()).is_none(), "{bad:?}");
        }
        let ok = format!("a/{}", "x".repeat(MAX_DIRECT_MODEL_LEN));
        assert!(select(&s, &ok, WireFormat::OpenAi, &empty_rotation()).is_some());
    }

    #[test]
    fn direct_model_allowlist_covers_default_pool_and_discovered_models() {
        let s = snap();
        let discovered: DiscoveredModelsMap = Arc::new(dashmap::DashMap::new());
        let a = &s.providers[0];
        assert!(direct_model_allowed(&s, &discovered, a, "m"), "provider default");
        assert!(!direct_model_allowed(&s, &discovered, a, "expensive"));
        discovered.insert("a".into(), vec!["expensive".into()]);
        assert!(direct_model_allowed(&s, &discovered, a, "expensive"), "discovered");
        assert!(!direct_model_allowed(&s, &discovered, a, "other"));
    }

    #[test]
    fn direct_provider_addressing_rejects_an_unknown_provider() {
        assert!(select(&snap(), "nope/some-model", WireFormat::OpenAi, &empty_rotation()).is_none());
    }

    #[test]
    fn direct_provider_addressing_translates_a_wire_format_mismatch() {
        // "a" is an OpenAi-wire-format passthrough provider; since
        // `HttpAdapter` now translates, direct addressing from the
        // Anthropic route still resolves to it rather than falling through.
        let s = snap();
        let sel = select(&s, "a/some-model", WireFormat::Anthropic, &empty_rotation()).unwrap();
        assert_eq!(sel.providers[0].provider.id, "a");
    }

    #[test]
    fn direct_codex_provider_addressing_supports_both_wire_formats() {
        let mut s = snap();
        s.providers[0].kind = ProviderKind::OauthCodex;
        for wire in [WireFormat::OpenAi, WireFormat::Anthropic] {
            let sel = select(&s, "a/gpt-5-codex", wire, &empty_rotation()).unwrap();
            assert_eq!(sel.providers[0].provider.id, "a");
            assert_eq!(sel.providers[0].effective_model, "gpt-5-codex");
        }
    }

    #[test]
    fn priority_strategy_never_rotates() {
        // Regression guard: default behavior for every pre-existing pool
        // must stay byte-for-byte identical regardless of how many times
        // select() has been called before.
        let s = snap();
        let rotation = empty_rotation();
        for _ in 0..5 {
            let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
            let ids: Vec<&str> = sel.providers.iter().map(|m| m.provider.id.as_str()).collect();
            assert_eq!(ids, vec!["a", "b"]);
        }
    }

    #[test]
    fn round_robin_rotates_start_index_on_each_call() {
        // sticky_limit: None normalizes to 1 - rotate every call.
        let s = snap_with_strategy(PoolStrategy::RoundRobin, None);
        let rotation = empty_rotation();

        let ids = |sel: &Selection| -> Vec<String> {
            sel.providers.iter().map(|m| m.provider.id.clone()).collect()
        };

        let sel1 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(ids(&sel1), vec!["a", "b"], "first call: priority order unchanged, cursor starts at 0");

        let sel2 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(ids(&sel2), vec!["b", "a"], "second call: rotated head");

        let sel3 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(ids(&sel3), vec!["a", "b"], "third call: wraps back around");
    }

    #[test]
    fn round_robin_respects_sticky_limit() {
        let s = snap_with_strategy(PoolStrategy::RoundRobin, Some(3));
        let rotation = empty_rotation();

        let ids = |sel: &Selection| -> Vec<String> {
            sel.providers.iter().map(|m| m.provider.id.clone()).collect()
        };

        let sel1 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        let sel2 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        let sel3 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(ids(&sel1), vec!["a", "b"]);
        assert_eq!(ids(&sel2), vec!["a", "b"], "same head for 3 consecutive calls (sticky_limit)");
        assert_eq!(ids(&sel3), vec!["a", "b"]);

        let sel4 = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(ids(&sel4), vec!["b", "a"], "4th call rotates to the next member");
    }

    #[test]
    fn round_robin_cursor_wraps_when_member_removed() {
        // Simulate a cursor left pointing past the end of a since-shrunk
        // member list (e.g. a member was deleted after the cursor advanced
        // past index 0). select() must still return a valid full-length
        // vec via `% members.len()`, not panic or truncate.
        let s = snap_with_strategy(PoolStrategy::RoundRobin, None);
        let rotation = empty_rotation();
        rotation.insert(
            "gpt-4o".to_string(),
            crate::core::state::RotationState { index: 47, consecutive_uses: 0 },
        );

        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
        assert_eq!(sel.providers.len(), 2, "full member list still returned");
        let ids: Vec<&str> = sel.providers.iter().map(|m| m.provider.id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"b"));
    }

    #[test]
    fn round_robin_is_a_no_op_for_a_single_member_pool() {
        let mut s = snap_with_strategy(PoolStrategy::RoundRobin, None);
        s.pools[0].members.retain(|m| m.provider_id == "a");
        let rotation = empty_rotation();

        for _ in 0..3 {
            let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &rotation).unwrap();
            assert_eq!(sel.providers.len(), 1);
            assert_eq!(sel.providers[0].provider.id, "a");
        }
    }

    #[test]
    fn dataset_logging_enabled_prefers_member_override_over_provider_default() {
        let mut p = prov("x");
        p.dataset_logging = false;
        assert!(dataset_logging_enabled(&p, Some(true)));
        p.dataset_logging = true;
        assert!(!dataset_logging_enabled(&p, Some(false)));
        p.dataset_logging = true;
        assert!(dataset_logging_enabled(&p, None));
    }

    #[test]
    fn select_carries_the_member_override_for_a_pool_routed_call() {
        let mut s = snap();
        s.pools[0].members[0].dataset_logging_override = Some(true);
        // s.pools[0].members[0] is "b" (priority 20) per snap()'s member
        // order; find "b" explicitly rather than relying on array order.
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let b = sel.providers.iter().find(|m| m.provider.id == "b").unwrap();
        assert_eq!(b.dataset_logging_override, Some(true));
    }

    #[test]
    fn select_direct_provider_always_yields_no_override() {
        let mut s = snap();
        s.providers[0].dataset_logging = true; // "a"
        let sel = select(&s, "a/some-model", WireFormat::OpenAi, &empty_rotation()).unwrap();
        assert_eq!(sel.providers[0].dataset_logging_override, None);
        assert_eq!(sel.providers[0].reasoning_effort_override, None);
    }

    #[test]
    fn resolve_reasoning_effort_prefers_member_override_over_provider_default() {
        let mut p = prov("x");
        p.default_reasoning_effort = Some(EffortLevel::Low);
        assert_eq!(
            resolve_reasoning_effort(&p, Some(EffortLevel::High)),
            Some(EffortLevel::High)
        );
        assert_eq!(
            resolve_reasoning_effort(&p, None),
            Some(EffortLevel::Low),
            "no member override falls back to the provider default"
        );
        p.default_reasoning_effort = None;
        assert_eq!(resolve_reasoning_effort(&p, None), None);
        assert_eq!(
            resolve_reasoning_effort(&p, Some(EffortLevel::Medium)),
            Some(EffortLevel::Medium),
            "a member override applies even with no provider default"
        );
    }

    #[test]
    fn select_carries_the_reasoning_effort_override_for_a_pool_routed_call() {
        let mut s = snap();
        s.pools[0].members[0].reasoning_effort_override = Some(EffortLevel::High);
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let b = sel.providers.iter().find(|m| m.provider.id == "b").unwrap();
        assert_eq!(b.reasoning_effort_override, Some(EffortLevel::High));
        let a = sel.providers.iter().find(|m| m.provider.id == "a").unwrap();
        assert_eq!(a.reasoning_effort_override, None);
    }

    fn image_snap() -> ConfigSnapshot {
        let mut s = snap();
        s.providers[0].kind = ProviderKind::OauthCodex; // "a"
        let member = |provider: &str, priority: i64, model: Option<&str>| PoolMember {
            pool_id: "img".into(), provider_id: provider.into(), priority,
            model_override: model.map(Into::into), dataset_logging_override: None,
            reasoning_effort_override: None,
        };
        s.pools.push(PoolWithMembers {
            pool: Pool {
                id: "img".into(), wire_format: WireFormat::OpenAi, created_at: Utc::now(),
                strategy: PoolStrategy::RoundRobin, sticky_limit: None, modality: Modality::Image,
            },
            members: vec![
                member("a", 10, Some("gpt-image-2")),
                // passthrough provider and a chat-model override: both skipped
                member("b", 20, Some("gpt-image-2")),
                member("a", 30, Some("gpt-5.5")),
            ],
        });
        s
    }

    #[test]
    fn image_model_rule() {
        for ok in ["gpt-image-2", "gpt-image-1.5", "gpt-5.5-image"] {
            assert!(is_image_model(ok), "{ok}");
        }
        for bad in ["gpt-image-", "-image", "gpt-5.5", "dall-e-3", "gpt image-2", ""] {
            assert!(!is_image_model(bad), "{bad:?}");
        }
    }

    #[test]
    fn chat_select_never_returns_an_image_pool_and_does_not_rotate_it() {
        let s = image_snap();
        let rotation = empty_rotation();
        assert!(select(&s, "img", WireFormat::OpenAi, &rotation).is_none());
        assert!(rotation.get("img").is_none(), "rejected selection must not touch the cursor");
    }

    #[test]
    fn image_select_rejects_chat_pools_without_rotating() {
        let s = snap_with_strategy(PoolStrategy::RoundRobin, None);
        let rotation = empty_rotation();
        assert!(select_image(&s, "gpt-4o", &rotation).is_none());
        assert!(rotation.get("gpt-4o").is_none());
    }

    #[test]
    fn image_select_keeps_only_codex_members_with_an_image_override() {
        let s = image_snap();
        let sel = select_image(&s, "img", &empty_rotation()).unwrap();
        assert_eq!(sel.providers.len(), 1);
        assert_eq!(sel.providers[0].provider.id, "a");
        assert_eq!(sel.providers[0].effective_model, "gpt-image-2");
        assert_eq!(sel.providers[0].dataset_logging_override, Some(false));
    }

    #[test]
    fn image_direct_addressing_needs_codex_kind_and_image_model() {
        let s = image_snap();
        let rotation = empty_rotation();
        let sel = select_image(&s, "a/gpt-5.5-image", &rotation).unwrap();
        assert!(sel.pool.is_none());
        assert_eq!(sel.providers[0].effective_model, "gpt-5.5-image");
        assert!(select_image(&s, "a/gpt-5.5", &rotation).is_none(), "chat model");
        assert!(select_image(&s, "b/gpt-image-2", &rotation).is_none(), "passthrough provider");
    }

    #[test]
    fn direct_model_allowlist_ignores_image_pool_overrides() {
        let s = image_snap();
        let discovered: DiscoveredModelsMap = Arc::new(dashmap::DashMap::new());
        let a = &s.providers[0];
        assert!(
            !direct_model_allowed(&s, &discovered, a, "gpt-image-2"),
            "an image pool member must not make its model chat-addressable"
        );
    }
}
