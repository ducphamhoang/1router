use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type LoginAttemptMap = Arc<DashMap<IpAddr, AttemptState>>;

pub const FAILURE_THRESHOLD: u32 = 5;
pub const MAX_LOCKOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default)]
pub struct AttemptState {
    pub failures: u32,
    pub locked_until: Option<Instant>,
}

/// Mirrors proxy::backoff::cooldown_for: 2s * 2^(n-1), capped.
pub fn cooldown_for(failures_over_threshold: u32) -> Duration {
    let level = failures_over_threshold.max(1);
    let secs = 2u64.saturating_mul(2u64.saturating_pow((level - 1).min(15) as u32));
    Duration::from_secs(secs).min(MAX_LOCKOUT)
}

pub fn is_locked_out(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) -> bool {
    map.get(&ip)
        .map(|s| matches!(s.locked_until, Some(until) if now < until))
        .unwrap_or(false)
}

pub fn record_failure(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) {
    let mut entry = map.entry(ip).or_default();
    entry.failures += 1;
    // >= not >: lock starting at the 5th recorded failure so the 6th *attempt*
    // is the one that gets blocked (matches the spec's "after 5 failures" and
    // the test below - review fix for an off-by-one caught in the Opus pass).
    if entry.failures >= FAILURE_THRESHOLD {
        let cooldown = cooldown_for(entry.failures - FAILURE_THRESHOLD);
        entry.locked_until = Some(now + cooldown);
    }
}

pub fn record_success(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr) {
    map.entry(ip).and_modify(|s| {
        s.failures = 0;
        s.locked_until = None;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn is_locked_out_false_before_threshold() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..4 {
            record_failure(&map, ip(1), now);
        }

        assert!(!is_locked_out(&map, ip(1), now));
        assert_eq!(map.get(&ip(1)).unwrap().failures, 4);
    }

    #[test]
    fn is_locked_out_true_once_threshold_exceeded() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }

        assert!(is_locked_out(&map, ip(1), now));
    }

    #[test]
    fn lockout_duration_escalates_and_caps_at_five_minutes() {
        assert_eq!(cooldown_for(1), Duration::from_secs(2));
        assert_eq!(cooldown_for(2), Duration::from_secs(4));
        assert_eq!(cooldown_for(3), Duration::from_secs(8));
        assert_eq!(cooldown_for(99), MAX_LOCKOUT);
    }

    #[test]
    fn record_success_resets_failures_and_clears_lockout() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }
        assert!(is_locked_out(&map, ip(1), now));

        record_success(&map, ip(1));

        let state = map.get(&ip(1)).unwrap();
        assert_eq!(state.failures, 0);
        assert!(state.locked_until.is_none());
    }

    #[test]
    fn different_ips_tracked_independently() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }

        assert!(is_locked_out(&map, ip(1), now));
        assert!(!is_locked_out(&map, ip(2), now));
    }
}
