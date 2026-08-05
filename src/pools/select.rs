use crate::core::model::{Pool, Provider, WireFormat};
use crate::core::state::ConfigSnapshot;

pub struct Selection<'a> {
    /// `None` for a direct `<provider_id>/<model>` selection (see
    /// `select_direct_provider` below) - there's no real pool row behind it.
    pub pool: Option<&'a Pool>,
    /// (provider, effective upstream model) pairs, in priority order. The
    /// effective model is the member's `model_override` if set, else the
    /// provider's own `upstream_model` - this is what lets one provider
    /// (one credential set) be shared across pools that each call a
    /// different model.
    pub providers: Vec<(&'a Provider, String)>,
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
) -> Option<Selection<'a>> {
    if let Some(pwm) = snapshot.pools.iter().find(|p| p.pool.id == pool_id) {
        if pwm.pool.wire_format != wire {
            return None;
        }

        let mut members = pwm.members.clone();
        members.sort_by_key(|m| m.priority);

        let providers = members
            .iter()
            .filter_map(|m| {
                let provider = snapshot.providers.iter().find(|p| p.id == m.provider_id)?;
                let model = m
                    .model_override
                    .clone()
                    .unwrap_or_else(|| provider.upstream_model.clone());
                Some((provider, model))
            })
            .collect();

        return Some(Selection {
            pool: Some(&pwm.pool),
            providers,
        });
    }

    select_direct_provider(snapshot, pool_id, wire)
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
        providers: vec![(provider, model.to_string())],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Pool, PoolMember, PoolWithMembers, Provider, ProviderKind, WireFormat};
    use crate::core::state::ConfigSnapshot;
    use chrono::Utc;

    fn prov(id: &str) -> Provider {
        Provider {
            id: id.into(), name: id.into(), wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough, base_url: Some("u".into()),
            api_key: Some("k".into()), upstream_model: "m".into(),
            created_at: Utc::now(), updated_at: Utc::now(),
        }
    }

    fn snap() -> ConfigSnapshot {
        ConfigSnapshot {
            providers: vec![prov("a"), prov("b")],
            pools: vec![PoolWithMembers {
                pool: Pool { id: "gpt-4o".into(), wire_format: WireFormat::OpenAi, created_at: Utc::now() },
                members: vec![
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "b".into(), priority: 20, model_override: None },
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "a".into(), priority: 10, model_override: None },
                ],
            }],
        }
    }

    #[test]
    fn orders_by_priority_ascending() {
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi).unwrap();
        let ids: Vec<&str> = sel.providers.iter().map(|(p, _)| p.id.as_str()).collect();
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
        });
        // dedupe: replace the "a" member from `snap()` with the overridden one
        s.pools[0].members.retain(|m| m.provider_id != "a" || m.model_override.is_some());

        let sel = select(&s, "gpt-4o", WireFormat::OpenAi).unwrap();
        let (_, model) = sel.providers.iter().find(|(p, _)| p.id == "a").unwrap();
        assert_eq!(model, "gpt-5.6-sol");

        let (_, model_b) = sel.providers.iter().find(|(p, _)| p.id == "b").unwrap();
        assert_eq!(model_b, "m", "falls back to the provider's own upstream_model when unset");
    }

    #[test]
    fn wrong_wire_format_returns_none() {
        assert!(select(&snap(), "gpt-4o", WireFormat::Anthropic).is_none());
    }

    #[test]
    fn missing_pool_returns_none() {
        assert!(select(&snap(), "nope", WireFormat::OpenAi).is_none());
    }

    #[test]
    fn direct_provider_slash_model_routes_to_that_provider_with_that_model() {
        let s = snap();
        let sel = select(&s, "a/some-other-model", WireFormat::OpenAi).unwrap();
        assert!(sel.pool.is_none());
        assert_eq!(sel.providers.len(), 1);
        let (provider, model) = &sel.providers[0];
        assert_eq!(provider.id, "a");
        assert_eq!(model, "some-other-model");
    }

    #[test]
    fn direct_provider_addressing_only_splits_on_the_first_slash() {
        let s = snap();
        let sel = select(&s, "a/meta-llama/Llama-3-70b", WireFormat::OpenAi).unwrap();
        let (provider, model) = &sel.providers[0];
        assert_eq!(provider.id, "a");
        assert_eq!(model, "meta-llama/Llama-3-70b");
    }

    #[test]
    fn direct_provider_addressing_is_only_a_fallback_a_real_pool_still_wins() {
        // "gpt-4o" is a real pool with no '/' - direct addressing never
        // applies here regardless.
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi).unwrap();
        assert!(sel.pool.is_some());
    }

    #[test]
    fn direct_provider_addressing_rejects_an_unknown_provider() {
        assert!(select(&snap(), "nope/some-model", WireFormat::OpenAi).is_none());
    }

    #[test]
    fn direct_provider_addressing_translates_a_wire_format_mismatch() {
        // "a" is an OpenAi-wire-format passthrough provider; since
        // `HttpAdapter` now translates, direct addressing from the
        // Anthropic route still resolves to it rather than falling through.
        let s = snap();
        let sel = select(&s, "a/some-model", WireFormat::Anthropic).unwrap();
        assert_eq!(sel.providers[0].0.id, "a");
    }

    #[test]
    fn direct_codex_provider_addressing_supports_both_wire_formats() {
        let mut s = snap();
        s.providers[0].kind = ProviderKind::OauthCodex;
        for wire in [WireFormat::OpenAi, WireFormat::Anthropic] {
            let sel = select(&s, "a/gpt-5-codex", wire).unwrap();
            assert_eq!(sel.providers[0].0.id, "a");
            assert_eq!(sel.providers[0].1, "gpt-5-codex");
        }
    }
}
