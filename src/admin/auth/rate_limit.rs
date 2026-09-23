use dashmap::DashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

pub use crate::core::state::{AttemptState, LoginAttemptMap};

pub const FAILURE_THRESHOLD: u32 = 5;
pub const MAX_LOCKOUT: Duration = Duration::from_secs(5 * 60);
/// An unlocked entry with no failure for this long is forgotten (SEC-19).
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Mirrors proxy::backoff::cooldown_for: 2s * 2^(n-1), capped.
pub fn cooldown_for(failures_over_threshold: u32) -> Duration {
    let level = failures_over_threshold.max(1);
    let secs = 2u64.saturating_mul(2u64.saturating_pow((level - 1).min(15) as u32));
    Duration::from_secs(secs).min(MAX_LOCKOUT)
}

/// The bucket an address is limited under: IPv4 as-is (including
/// IPv4-mapped IPv6), IPv6 by its /64 - one host usually controls a whole
/// /64, so per-address limiting would be trivially sidestepped (SEC-05).
pub fn limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

pub fn is_locked_out(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) -> bool {
    map.get(&limit_key(ip))
        .map(|s| matches!(s.locked_until, Some(until) if now < until))
        .unwrap_or(false)
}

/// Releases a reserved attempt slot when dropped, whatever the outcome.
pub struct AttemptGuard<'a> {
    map: &'a DashMap<IpAddr, AttemptState>,
    key: IpAddr,
}

impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut s) = self.map.get_mut(&self.key) {
            s.in_flight = s.in_flight.saturating_sub(1);
        }
    }
}

/// Check the lockout *and* reserve an attempt in one step, under the
/// entry's lock. Checking first and recording the failure only after the
/// (slow) password verify let a concurrent burst all pass the check before
/// the first failure landed (SEC-05). Now at most `FAILURE_THRESHOLD`
/// attempts can be outstanding-or-failed per bucket, and once the threshold
/// is reached only one attempt at a time is let through after each lockout.
pub fn try_begin_attempt(
    map: &DashMap<IpAddr, AttemptState>,
    ip: IpAddr,
    now: Instant,
) -> Option<AttemptGuard<'_>> {
    let key = limit_key(ip);
    let mut entry = map.entry(key).or_default();
    if matches!(entry.locked_until, Some(until) if now < until) {
        return None;
    }
    let allowed = if entry.failures >= FAILURE_THRESHOLD {
        entry.in_flight == 0
    } else {
        entry.failures + entry.in_flight < FAILURE_THRESHOLD
    };
    if !allowed {
        return None;
    }
    entry.in_flight += 1;
    drop(entry);
    Some(AttemptGuard { map, key })
}

pub fn record_failure(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) {
    let mut entry = map.entry(limit_key(ip)).or_default();
    entry.failures += 1;
    entry.last_failure = Some(now);
    // >= not >: lock starting at the 5th recorded failure so the 6th *attempt*
    // is the one that gets blocked (matches the spec's "after 5 failures" and
    // the test below - review fix for an off-by-one caught in the Opus pass).
    if entry.failures >= FAILURE_THRESHOLD {
        let cooldown = cooldown_for(entry.failures - FAILURE_THRESHOLD);
        entry.locked_until = Some(now + cooldown);
    }
}

pub fn record_success(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr) {
    map.entry(limit_key(ip)).and_modify(|s| {
        s.failures = 0;
        s.locked_until = None;
    });
}

/// Forget buckets that are idle: not locked, nothing in flight, and no
/// failure within [`STALE_AFTER`] (SEC-19). Returns how many were removed.
pub fn prune_stale(map: &DashMap<IpAddr, AttemptState>, now: Instant) -> usize {
    let before = map.len();
    map.retain(|_, s| {
        let locked = matches!(s.locked_until, Some(until) if now < until);
        let recent = matches!(s.last_failure, Some(t) if now < t + STALE_AFTER);
        locked || recent || s.in_flight > 0
    });
    before - map.len()
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
    fn concurrent_attempts_are_capped_at_the_threshold() {
        let map = DashMap::new();
        let now = Instant::now();
        let guards: Vec<_> = (0..FAILURE_THRESHOLD)
            .map(|_| try_begin_attempt(&map, ip(1), now).expect("under threshold"))
            .collect();
        assert!(try_begin_attempt(&map, ip(1), now).is_none(), "burst must not exceed the threshold");
        drop(guards);
        assert!(try_begin_attempt(&map, ip(1), now).is_some(), "slots free again once attempts finish");
    }

    #[test]
    fn after_the_threshold_only_one_attempt_per_lockout_window() {
        let map = DashMap::new();
        let now = Instant::now();
        for _ in 0..FAILURE_THRESHOLD {
            record_failure(&map, ip(1), now);
        }
        assert!(try_begin_attempt(&map, ip(1), now).is_none(), "locked");
        let later = now + MAX_LOCKOUT;
        let first = try_begin_attempt(&map, ip(1), later);
        assert!(first.is_some());
        assert!(try_begin_attempt(&map, ip(1), later).is_none(), "one at a time");
    }

    #[test]
    fn ipv6_is_limited_per_slash_64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(limit_key(a), limit_key(b));
        assert_ne!(limit_key(a), limit_key(c));
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(limit_key(mapped), ip(1));
    }

    #[test]
    fn prune_stale_drops_only_idle_buckets() {
        let map = DashMap::new();
        let now = Instant::now();
        record_failure(&map, ip(1), now);
        map.insert(ip(2), AttemptState::default());
        assert_eq!(prune_stale(&map, now), 1);
        assert!(map.contains_key(&ip(1)));
        assert_eq!(prune_stale(&map, now + STALE_AFTER), 1);
        assert!(map.is_empty());
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
