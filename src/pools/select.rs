use crate::core::model::{Pool, Provider, WireFormat};
use crate::core::state::ConfigSnapshot;

pub struct Selection<'a> {
    pub pool: &'a Pool,
    pub providers: Vec<&'a Provider>,
}

fn wire_eq(a: WireFormat, b: WireFormat) -> bool {
    matches!(
        (a, b),
        (WireFormat::OpenAi, WireFormat::OpenAi) | (WireFormat::Anthropic, WireFormat::Anthropic)
    )
}

pub fn select<'a>(
    snapshot: &'a ConfigSnapshot,
    pool_id: &str,
    wire: WireFormat,
) -> Option<Selection<'a>> {
    let pwm = snapshot.pools.iter().find(|p| p.pool.id == pool_id)?;
    if !wire_eq(pwm.pool.wire_format, wire) {
        return None;
    }

    let mut members = pwm.members.clone();
    members.sort_by_key(|m| m.priority);

    let providers = members
        .iter()
        .filter_map(|m| snapshot.providers.iter().find(|p| p.id == m.provider_id))
        .collect();

    Some(Selection {
        pool: &pwm.pool,
        providers,
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
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "b".into(), priority: 20 },
                    PoolMember { pool_id: "gpt-4o".into(), provider_id: "a".into(), priority: 10 },
                ],
            }],
        }
    }

    #[test]
    fn orders_by_priority_ascending() {
        let s = snap();
        let sel = select(&s, "gpt-4o", WireFormat::OpenAi).unwrap();
        let ids: Vec<&str> = sel.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn wrong_wire_format_returns_none() {
        assert!(select(&snap(), "gpt-4o", WireFormat::Anthropic).is_none());
    }

    #[test]
    fn missing_pool_returns_none() {
        assert!(select(&snap(), "nope", WireFormat::OpenAi).is_none());
    }
}
