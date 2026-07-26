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

    #[test]
    fn verify_rejects_malformed_hash_string_without_panicking() {
        assert!(!verify_password("not-a-real-hash", "x"));
    }
}
