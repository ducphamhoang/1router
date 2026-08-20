use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderStatus {
    Healthy,
    Cooling,
    Misconfigured,
}

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
        if matches!(self.status, ProviderStatus::Misconfigured) {
            return false;
        }
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

    pub fn mark_misconfigured(&mut self) {
        self.status = ProviderStatus::Misconfigured;
        self.unavailable_until = None;
    }
}

pub type RuntimeStateMap = Arc<dashmap::DashMap<String, ProviderRuntimeState>>;

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
    fn misconfigured_is_never_available() {
        let mut s = ProviderRuntimeState::default();
        s.mark_misconfigured();
        assert!(matches!(s.status, ProviderStatus::Misconfigured));
        assert!(!s.is_available(Instant::now() + Duration::from_secs(999_999)));
    }
}
