//! Prompt-cache diagnostics for Codex requests: which (hashed) cache key a
//! request used and how long since the previous request with that same key,
//! logged next to the cached-token count at debug level. Cached prefixes need
//! a few seconds to become usable, so the gap explains most "misses".
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const MAX_KEYS: usize = 4096;
const KEEP: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
pub struct CacheTrace {
    /// Short hash of the cache key (never the key itself).
    pub key: String,
    /// Seconds since the previous request that used the same key.
    pub gap_secs: Option<u64>,
}

fn seen() -> &'static Mutex<HashMap<String, Instant>> {
    static SEEN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

pub fn note(cache_key: &str) -> CacheTrace {
    let key: String = Sha256::digest(cache_key.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    let now = Instant::now();
    let mut map = seen().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() >= MAX_KEYS {
        map.retain(|_, t| now.duration_since(*t) < KEEP);
        if map.len() >= MAX_KEYS {
            map.clear();
        }
    }
    let gap_secs = map.insert(key.clone(), now).map(|t| now.duration_since(t).as_secs());
    CacheTrace { key, gap_secs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_use_has_no_gap_and_key_is_hashed() {
        let t = note("unit-test-key-a");
        assert!(t.gap_secs.is_none());
        assert_eq!(t.key.len(), 8);
        assert!(!t.key.contains("unit"));
        assert_eq!(note("unit-test-key-a").gap_secs, Some(0));
        assert!(note("unit-test-key-b").gap_secs.is_none());
    }
}
