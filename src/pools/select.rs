use crate::core::model::{Pool, PoolMember, PoolStrategy, Provider, WireFormat};
use crate::core::state::{ConfigSnapshot, PoolRotationMap};

pub struct Selection<'a> {
    /// `None` for a direct `<provider_id>/<model>` selection (see
    /// `select_direct_provider` below) - there's no real pool row behind it.
    pub pool: Option<&'a Pool>,
    /// (provider, effective upstream model, resolved dataset-logging
    /// member override) triples, in priority order. The effective model is
    /// the member's `model_override` if set, else the provider's own
    /// `upstream_model` - this is what lets one provider (one credential
    /// set) be shared across pools that each call a different model. The
    /// third element is `PoolMember.dataset_logging_override` for a
    /// pool-routed entry, or `None` for a direct-provider-addressed one
    /// (which has no `PoolMember` row at all) - either way, pass it to
    /// `dataset_logging_enabled` alongside the provider to resolve the
    /// effective setting.
    pub providers: Vec<(&'a Provider, String, Option<bool>)>,
}

/// `member_override` is `PoolMember.dataset_logging_override` for a
/// pool-routed call, or `None` for direct-provider addressing (which has
/// no `PoolMember` row at all) - either way, `None` means "inherit the
/// provider's own setting".
pub fn dataset_logging_enabled(provider: &Provider, member_override: Option<bool>) -> bool {
    member_override.unwrap_or(provider.dataset_logging)
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
        if pwm.pool.wire_format != wire {
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
                Some((provider, model, m.dataset_logging_override))
            })
            .collect();

        return Some(Selection {
            pool: Some(&pwm.pool),
            providers,
        });
    }

    select_direct_provider(snapshot, pool_id, wire)
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

fn select_direct_provider<'a>(
    snapshot: &'a ConfigSnapshot,
    requested: &str,
    _wire: WireFormat,
) -> Option<Selection<'a>> {
    let (provider_id, model) = requested.split_once('/')?;
    let provider = snapshot.providers.iter().find(|p| p.id == provider_id)?;
    Some(Selection {
        pool: None,
        providers: vec![(provider, model.to_string(), None)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Pool, PoolMember, PoolWithMembers, Provider, ProviderKind, WireFormat};
    use crate::core::state::{ConfigSnapshot, PoolRotationMap};
    use chrono::Utc;
    use std::sync::Arc;

    fn prov(id: &str) -> Provider {
        Provider {
            id: id.into(), name: id.into(), wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough, base_url: Some("u".into()),
            api_key: Some("k".into()), upstream_model: "m".into(),
            dataset_logging: false,
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
                    strategy, sticky_limit,
                },
                members: vec![
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "b".into(), priority: 20, model_override: None, dataset_logging_override: None },
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "a".into(), priority: 10, model_override: None, dataset_logging_override: None },
                ],
            }],
        }
    }

    #[test]
    fn orders_by_priority_ascending() {
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let ids: Vec<&str> = sel.providers.iter().map(|(p, _, _)| p.id.as_str()).collect();
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
        });
        // dedupe: replace the "a" member from `snap()` with the overridden one
        s.pools[0].members.retain(|m| m.provider_id != "a" || m.model_override.is_some());

        let sel = select(&s, "gpt-4o", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let (_, model, _) = sel.providers.iter().find(|(p, _, _)| p.id == "a").unwrap();
        assert_eq!(model, "gpt-5.6-sol");

        let (_, model_b, _) = sel.providers.iter().find(|(p, _, _)| p.id == "b").unwrap();
        assert_eq!(model_b, "m", "falls back to the provider's own upstream_model when unset");
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
        let (provider, model, _) = &sel.providers[0];
        assert_eq!(provider.id, "a");
        assert_eq!(model, "some-other-model");
    }

    #[test]
    fn direct_provider_addressing_only_splits_on_the_first_slash() {
        let s = snap();
        let sel = select(&s, "a/meta-llama/Llama-3-70b", WireFormat::OpenAi, &empty_rotation()).unwrap();
        let (provider, model, _) = &sel.providers[0];
        assert_eq!(provider.id, "a");
        assert_eq!(model, "meta-llama/Llama-3-70b");
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
        assert_eq!(sel.providers[0].0.id, "a");
    }

    #[test]
    fn direct_codex_provider_addressing_supports_both_wire_formats() {
        let mut s = snap();
        s.providers[0].kind = ProviderKind::OauthCodex;
        for wire in [WireFormat::OpenAi, WireFormat::Anthropic] {
            let sel = select(&s, "a/gpt-5-codex", wire, &empty_rotation()).unwrap();
            assert_eq!(sel.providers[0].0.id, "a");
            assert_eq!(sel.providers[0].1, "gpt-5-codex");
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
            let ids: Vec<&str> = sel.providers.iter().map(|(p, _, _)| p.id.as_str()).collect();
            assert_eq!(ids, vec!["a", "b"]);
        }
    }

    #[test]
    fn round_robin_rotates_start_index_on_each_call() {
        // sticky_limit: None normalizes to 1 - rotate every call.
        let s = snap_with_strategy(PoolStrategy::RoundRobin, None);
        let rotation = empty_rotation();

        let ids = |sel: &Selection| -> Vec<String> {
            sel.providers.iter().map(|(p, _, _)| p.id.clone()).collect()
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
            sel.providers.iter().map(|(p, _, _)| p.id.clone()).collect()
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
        let ids: Vec<&str> = sel.providers.iter().map(|(p, _, _)| p.id.as_str()).collect();
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
            assert_eq!(sel.providers[0].0.id, "a");
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
        let (_, _, member_override) = sel.providers.iter().find(|(p, _, _)| p.id == "b").unwrap();
        assert_eq!(*member_override, Some(true));
    }

    #[test]
    fn select_direct_provider_always_yields_no_override() {
        let mut s = snap();
        s.providers[0].dataset_logging = true; // "a"
        let sel = select(&s, "a/some-model", WireFormat::OpenAi, &empty_rotation()).unwrap();
        assert_eq!(sel.providers[0].2, None);
    }
}
