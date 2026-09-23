use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderStatus {
    Healthy,
    Cooling,
    Misconfigured,
}

/// How long a `Misconfigured` (provider, model) is skipped before the next
/// request is allowed through to re-probe upstream (BL-01). A still-broken
/// credential just gets re-marked for another window; a recovered upstream
/// heals itself via `record_success`, with no admin action or restart.
pub const MISCONFIGURED_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct ProviderRuntimeState {
    pub backoff_level: u8,
    pub unavailable_until: Option<Instant>,
    pub status: ProviderStatus,
}

impl Default for ProviderRuntimeState {
    fn default() -> Self {
        ProviderRuntimeState {
            backoff_level: 0,
            unavailable_until: None,
            status: ProviderStatus::Healthy,
        }
    }
}

impl ProviderRuntimeState {
    pub fn is_available(&self, now: Instant) -> bool {
        match self.unavailable_until {
            Some(until) => now >= until,
            None => true,
        }
    }

    pub fn record_success(&mut self) {
        self.backoff_level = 0;
        self.unavailable_until = None;
        self.status = ProviderStatus::Healthy;
    }

    /// Force a provider back to healthy - used when an admin action
    /// (validate-model, provider update) proves the credentials/model work,
    /// so a stale `Misconfigured`/`Cooling` flag can't keep blocking the
    /// proxy path until a restart.
    pub fn reset_to_healthy(&mut self) {
        self.backoff_level = 0;
        self.unavailable_until = None;
        self.status = ProviderStatus::Healthy;
    }

    pub fn record_retryable(&mut self, cooldown: Duration, now: Instant) {
        self.backoff_level = self.backoff_level.saturating_add(1);
        self.unavailable_until = Some(now + cooldown);
        self.status = ProviderStatus::Cooling;
    }

    /// Skip this (provider, model) for [`MISCONFIGURED_RETRY_AFTER`]. Only
    /// for failures that are the *provider's* fault (rejected credential,
    /// dead refresh token) - never for a request the upstream rejected as
    /// malformed, which says nothing about the provider (SEC-01).
    pub fn mark_misconfigured(&mut self, now: Instant) {
        self.status = ProviderStatus::Misconfigured;
        self.unavailable_until = Some(now + MISCONFIGURED_RETRY_AFTER);
    }
}

pub type RuntimeStateMap = Arc<dashmap::DashMap<String, ProviderRuntimeState>>;

/// How long past its `unavailable_until` an entry is kept (so repeated
/// failures still escalate the backoff) before [`prune_idle`] drops it.
pub const RUNTIME_IDLE_RETENTION: Duration = Duration::from_secs(15 * 60);

/// Drop entries that carry no information any more: healthy ones (a missing
/// entry *is* healthy) and ones whose cooldown ended more than
/// `retention` ago. Keeps the map bounded by recent failures rather than by
/// every `(provider, model)` string ever seen (SEC-03). Returns how many
/// were removed.
pub fn prune_idle(map: &RuntimeStateMap, now: Instant, retention: Duration) -> usize {
    let before = map.len();
    map.retain(|_, st| match st.unavailable_until {
        Some(until) => now < until + retention,
        None => false,
    });
    before - map.len()
}

/// Reset every runtime-state entry belonging to `provider_id`, across all
/// of its models - used after a credential/config update proves the
/// provider works again. Before pool members could carry their own model
/// (`migrations/0005_pool_member_model_identity.sql`), a provider had
/// exactly one runtime-state entry keyed by its bare id; now it may have
/// one per `(provider_id, model)` pair it backs, so "reset this provider"
/// means "reset all of them", not just one lookup.
pub fn reset_provider_to_healthy(map: &RuntimeStateMap, provider_id: &str) {
    let prefix = runtime_key_prefix(provider_id);
    for mut entry in map.iter_mut() {
        if entry.key().starts_with(&prefix) {
            entry.value_mut().reset_to_healthy();
        }
    }
}

/// Mark every *currently tracked* `(provider_id, *)` entry misconfigured -
/// used when a failure is credential-level (e.g. a background OAuth
/// refresh discovers `invalid_grant`), which dooms every model that
/// credential backs, not just whichever one happened to trigger the
/// refresh. Models this provider backs but hasn't served a request for
/// yet have no entry to mark here; they self-detect the same dead
/// credential on their own first live request instead (via
/// `proxy::flow`'s own `AuthExpired` -> `RefreshError::InvalidGrant`
/// handling), so nothing is silently missed - just detected slightly
/// later for a model nobody has called yet.
pub fn mark_provider_misconfigured(map: &RuntimeStateMap, provider_id: &str) {
    let prefix = runtime_key_prefix(provider_id);
    let now = Instant::now();
    for mut entry in map.iter_mut() {
        if entry.key().starts_with(&prefix) {
            entry.value_mut().mark_misconfigured(now);
        }
    }
}

/// The single worst status across every `(provider_id, *)` entry, for
/// admin display where one provider row shows one status regardless of
/// how many models it backs. Ranks `Misconfigured` > `Cooling` >
/// `Healthy`; among same-rank entries, keeps the one with the higher
/// `backoff_level` (i.e. the most-troubled one), so the admin sees the
/// worst case rather than an arbitrary member's.
pub fn worst_provider_state(map: &RuntimeStateMap, provider_id: &str) -> Option<ProviderRuntimeState> {
    let prefix = runtime_key_prefix(provider_id);
    fn rank(status: ProviderStatus) -> u8 {
        match status {
            ProviderStatus::Healthy => 0,
            ProviderStatus::Cooling => 1,
            ProviderStatus::Misconfigured => 2,
        }
    }
    map.iter()
        .filter(|entry| entry.key().starts_with(&prefix))
        .map(|entry| entry.value().clone())
        .fold(None, |worst, candidate| match worst {
            None => Some(candidate),
            Some(current) => {
                if (rank(candidate.status), candidate.backoff_level)
                    > (rank(current.status), current.backoff_level)
                {
                    Some(candidate)
                } else {
                    Some(current)
                }
            }
        })
}

/// `RuntimeStateMap`'s key. Widened from bare `provider_id` so a failure
/// on one `(provider, model)` pair - e.g. one pool member's
/// `model_override`, per
/// `migrations/0005_pool_member_model_identity.sql` - doesn't cool down
/// or misconfigure its siblings sharing the same provider/credential.
/// `\u{1f}` (unit separator) can't appear in either half (provider ids are
/// validated path-id-like strings via `validate_path_id`; model names are
/// upstream identifiers, never containing control characters in practice),
/// so this is unambiguous with no escaping needed.
pub fn runtime_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\u{1f}{model}")
}

/// The prefix every `runtime_key` for a given provider starts with,
/// regardless of model - used to find/reset/aggregate every runtime-state
/// entry belonging to one provider (e.g. after a credential update, or for
/// admin status display) without knowing which models it currently backs.
pub fn runtime_key_prefix(provider_id: &str) -> String {
    format!("{provider_id}\u{1f}")
}

#[cfg(test)]
mod runtime_key_tests {
    use super::*;

    #[test]
    fn different_models_get_different_keys() {
        assert_ne!(
            runtime_key("p1", "model-a"),
            runtime_key("p1", "model-b"),
        );
    }

    #[test]
    fn key_starts_with_its_provider_prefix() {
        let key = runtime_key("p1", "model-a");
        assert!(key.starts_with(&runtime_key_prefix("p1")));
        // A different provider id must not share the prefix, even one
        // that's a plain-string prefix of it.
        assert!(!key.starts_with(&runtime_key_prefix("p")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn default_state_is_healthy_and_available() {
        let s = ProviderRuntimeState::default();
        assert_eq!(s.backoff_level, 0);
        assert!(matches!(s.status, ProviderStatus::Healthy));
        assert!(s.is_available(Instant::now()));
    }

    #[test]
    fn retryable_bumps_level_and_cools_down() {
        let now = Instant::now();
        let mut s = ProviderRuntimeState::default();
        s.record_retryable(Duration::from_secs(60), now);
        assert_eq!(s.backoff_level, 1);
        assert!(matches!(s.status, ProviderStatus::Cooling));
        assert!(!s.is_available(now));
        assert!(s.is_available(now + Duration::from_secs(61)));
    }

    #[test]
    fn success_clears_state() {
        let now = Instant::now();
        let mut s = ProviderRuntimeState::default();
        s.record_retryable(Duration::from_secs(60), now);
        s.record_success();
        assert_eq!(s.backoff_level, 0);
        assert!(matches!(s.status, ProviderStatus::Healthy));
        assert!(s.is_available(now));
    }

    #[test]
    fn prune_idle_keeps_only_recent_failures() {
        let map: RuntimeStateMap = Arc::new(dashmap::DashMap::new());
        let now = Instant::now();
        map.insert("healthy".into(), ProviderRuntimeState::default());
        let mut cooling = ProviderRuntimeState::default();
        cooling.record_retryable(Duration::from_secs(30), now);
        map.insert("cooling".into(), cooling);

        assert_eq!(prune_idle(&map, now, RUNTIME_IDLE_RETENTION), 1);
        assert!(map.contains_key("cooling"));
        let later = now + Duration::from_secs(30) + RUNTIME_IDLE_RETENTION;
        assert_eq!(prune_idle(&map, later, RUNTIME_IDLE_RETENTION), 1);
        assert!(map.is_empty());
    }

    #[test]
    fn misconfigured_is_skipped_then_re_probed_after_the_retry_window() {
        let mut s = ProviderRuntimeState::default();
        let now = Instant::now();
        s.mark_misconfigured(now);
        assert!(matches!(s.status, ProviderStatus::Misconfigured));
        assert!(!s.is_available(now));
        assert!(!s.is_available(now + MISCONFIGURED_RETRY_AFTER - Duration::from_secs(1)));
        assert!(s.is_available(now + MISCONFIGURED_RETRY_AFTER));

        // the re-probe succeeding heals it
        s.record_success();
        assert!(matches!(s.status, ProviderStatus::Healthy));
        assert!(s.is_available(now));
    }
}
