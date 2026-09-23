use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;

/// Argon2id via the crate's built-in default params (RFC-9106-recommended
/// low-memory profile) - deliberate, not hand-tuned.
pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))
}

/// Constant-time by construction (PasswordVerifier). Never panics on a
/// malformed `hash` string; returns false for untrusted DB content instead.
pub fn verify_password(hash: &str, plain: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

/// At most this many argon2 verifications run at once, on the blocking
/// pool - a flood of logins can't occupy the async workers that serve
/// `/v1/*` (SEC-05).
const MAX_CONCURRENT_VERIFIES: usize = 2;
static VERIFY_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_VERIFIES);

/// A valid argon2id hash of a random string, verified against when the
/// username doesn't match so both paths cost the same (SEC-20 timing).
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password(&uuid::Uuid::new_v4().to_string()).unwrap_or_default())
}

/// [`verify_password`] off the async runtime, behind a small global
/// semaphore. `hash = None` burns the same work against a dummy hash and
/// returns false.
pub async fn verify_password_async(hash: Option<String>, plain: String) -> bool {
    let Ok(_permit) = VERIFY_PERMITS.acquire().await else {
        return false;
    };
    tokio::task::spawn_blocking(move || match hash {
        Some(h) => verify_password(&h, &plain),
        None => {
            let _ = verify_password(dummy_hash(), &plain);
            false
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_round_trip() {
        let hash = hash_password("correct horse").unwrap();

        assert!(verify_password(&hash, "correct horse"));
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let hash = hash_password("correct horse").unwrap();

        assert!(!verify_password(&hash, "wrong"));
    }

    #[test]
    fn hash_is_randomized_per_call() {
        let first = hash_password("correct horse").unwrap();
        let second = hash_password("correct horse").unwrap();

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn async_verify_matches_sync_and_none_is_false() {
        let hash = hash_password("pw").unwrap();
        assert!(verify_password_async(Some(hash.clone()), "pw".into()).await);
        assert!(!verify_password_async(Some(hash), "nope".into()).await);
        assert!(!verify_password_async(None, "pw".into()).await);
    }

    #[test]
    fn verify_rejects_malformed_hash_string_without_panicking() {
        assert!(!verify_password("not-a-real-hash", "x"));
    }
}
