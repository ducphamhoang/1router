use axum::http::HeaderMap;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::time::Duration;

use crate::core::error::AppError;

pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct SessionRow {
    pub token_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct AdminSession {
    pub token_hash: String,
}

fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub async fn create_session(db: &SqlitePool) -> Result<(String, DateTime<Utc>), AppError> {
    use rand::RngCore;

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let raw_token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let token_hash = hash_token(&raw_token);
    let now = Utc::now();
    let expires_at = now + ChronoDuration::from_std(SESSION_TTL).unwrap();

    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
         VALUES (?, ?, ?)",
    )
    .bind(&token_hash)
    .bind(now)
    .bind(expires_at)
    .execute(db)
    .await?;

    Ok((raw_token, expires_at))
}

pub async fn validate_session(
    db: &SqlitePool,
    raw_token: &str,
) -> Result<Option<SessionRow>, AppError> {
    let hash = hash_token(raw_token);
    let row = sqlx::query_as::<_, SessionRow>(
        "SELECT token_hash, created_at, expires_at
         FROM admin_sessions
         WHERE token_hash = ?",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;

    Ok(row.filter(|r| r.expires_at > Utc::now()))
}

pub async fn renew_if_needed(
    db: &SqlitePool,
    token_hash: &str,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<(), AppError> {
    let now = Utc::now();
    let remaining = expires_at - now;
    let full_window = expires_at - created_at;
    if remaining * 2 > full_window {
        return Ok(());
    }

    let absolute_cap = created_at + ChronoDuration::from_std(ABSOLUTE_LIFETIME).unwrap();
    let candidate = now + ChronoDuration::from_std(SESSION_TTL).unwrap();
    let new_expiry = candidate.min(absolute_cap);
    if new_expiry <= expires_at {
        return Ok(());
    }

    sqlx::query("UPDATE admin_sessions SET expires_at = ? WHERE token_hash = ?")
        .bind(new_expiry)
        .bind(token_hash)
        .execute(db)
        .await?;

    Ok(())
}

pub async fn delete_all_sessions(db: &SqlitePool) -> Result<(), AppError> {
    sqlx::query("DELETE FROM admin_sessions").execute(db).await?;
    Ok(())
}

pub async fn delete_all_sessions_except(
    db: &SqlitePool,
    keep_token_hash: &str,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM admin_sessions WHERE token_hash != ?")
        .bind(keep_token_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_expired(db: &SqlitePool) -> Result<u64, AppError> {
    let res = sqlx::query("DELETE FROM admin_sessions WHERE expires_at < ?")
        .bind(Utc::now())
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

pub fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub fn cookie_name(is_https: bool) -> &'static str {
    if is_https {
        "__Host-admin_session"
    } else {
        "admin_session"
    }
}

pub fn build_set_cookie(raw_token: &str, expires_at: DateTime<Utc>, is_https: bool) -> String {
    let name = cookie_name(is_https);
    let max_age = (expires_at - Utc::now()).num_seconds().max(0);
    let mut cookie =
        format!("{name}={raw_token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}");
    if is_https {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn build_clear_cookie(is_https: bool) -> String {
    let name = cookie_name(is_https);
    let mut cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if is_https {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn extract_cookie<'a>(headers: &'a HeaderMap, is_https: bool) -> Option<&'a str> {
    let name = cookie_name(is_https);
    let raw = headers.get("cookie")?.to_str().ok()?;
    raw.split("; ").find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k == name).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use axum::http::{HeaderMap, HeaderValue};
    use chrono::{Duration as ChronoDuration, Utc};
    use sha2::{Digest, Sha256};

    async fn db() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_tests.db");
        let pool = init_pool(path.to_str().unwrap()).await.unwrap();
        std::mem::forget(dir);
        pool
    }

    fn hash_for_test(raw: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    #[tokio::test]
    async fn create_session_issues_lookupable_token() {
        let db = db().await;
        let (raw, expires_at) = create_session(&db).await.unwrap();

        assert_eq!(raw.len(), 64);
        assert!(expires_at > Utc::now());

        let row = validate_session(&db, &raw).await.unwrap().unwrap();
        assert_eq!(row.token_hash, hash_for_test(&raw));
    }

    #[tokio::test]
    async fn validate_session_rejects_unknown_token() {
        let db = db().await;
        let row = validate_session(&db, "not-a-real-token").await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn validate_session_rejects_expired_token() {
        let db = db().await;
        let raw = "expired-token";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(now - ChronoDuration::hours(2))
        .bind(now - ChronoDuration::hours(1))
        .execute(&db)
        .await
        .unwrap();

        let row = validate_session(&db, raw).await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn renew_if_needed_skips_write_when_over_half_ttl_remains() {
        let db = db().await;
        let (raw, _) = create_session(&db).await.unwrap();
        let before = validate_session(&db, &raw).await.unwrap().unwrap();

        renew_if_needed(&db, &before.token_hash, before.created_at, before.expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, &raw).await.unwrap().unwrap();
        assert_eq!(after.expires_at, before.expires_at);
    }

    #[tokio::test]
    async fn renew_if_needed_extends_when_under_half_ttl_remains() {
        let db = db().await;
        let raw = "needs-renewal";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();
        let created_at = now - ChronoDuration::hours(20);
        let expires_at = now + ChronoDuration::hours(1);

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(created_at)
        .bind(expires_at)
        .execute(&db)
        .await
        .unwrap();

        renew_if_needed(&db, &token_hash, created_at, expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, raw).await.unwrap().unwrap();
        assert!(after.expires_at > expires_at);
    }

    #[tokio::test]
    async fn renew_if_needed_never_exceeds_absolute_lifetime_cap() {
        let db = db().await;
        let raw = "near-cap";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();
        let created_at = now - ChronoDuration::hours(165);
        let expires_at = now + ChronoDuration::minutes(1);
        let cap = created_at + ChronoDuration::from_std(ABSOLUTE_LIFETIME).unwrap();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(created_at)
        .bind(expires_at)
        .execute(&db)
        .await
        .unwrap();

        renew_if_needed(&db, &token_hash, created_at, expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, raw).await.unwrap().unwrap();
        assert!(after.expires_at <= cap);
    }

    #[tokio::test]
    async fn delete_expired_removes_only_expired_rows() {
        let db = db().await;
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES ('expired', ?, ?), ('valid', ?, ?)",
        )
        .bind(now - ChronoDuration::hours(2))
        .bind(now - ChronoDuration::hours(1))
        .bind(now)
        .bind(now + ChronoDuration::hours(1))
        .execute(&db)
        .await
        .unwrap();

        let deleted = delete_expired(&db).await.unwrap();
        assert_eq!(deleted, 1);

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions ORDER BY token_hash")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(remaining, vec!["valid".to_string()]);
    }

    #[tokio::test]
    async fn delete_all_sessions_removes_everything() {
        let db = db().await;
        create_session(&db).await.unwrap();
        create_session(&db).await.unwrap();

        delete_all_sessions(&db).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn delete_all_sessions_except_keeps_only_named_token() {
        let db = db().await;
        let (keep_raw, _) = create_session(&db).await.unwrap();
        let (drop_raw, _) = create_session(&db).await.unwrap();
        let keep_hash = hash_for_test(&keep_raw);
        let drop_hash = hash_for_test(&drop_raw);

        delete_all_sessions_except(&db, &keep_hash).await.unwrap();

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(remaining, vec![keep_hash]);
        assert!(!remaining.contains(&drop_hash));
    }

    #[test]
    fn cookie_uses_host_prefix_and_secure_only_when_forwarded_proto_is_https() {
        let mut https_headers = HeaderMap::new();
        https_headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let http_headers = HeaderMap::new();
        let expires = Utc::now() + ChronoDuration::hours(1);

        assert!(is_https(&https_headers));
        assert!(!is_https(&http_headers));
        assert_eq!(cookie_name(true), "__Host-admin_session");
        assert_eq!(cookie_name(false), "admin_session");

        let secure = build_set_cookie("tok123", expires, true);
        assert!(secure.starts_with("__Host-admin_session=tok123"));
        assert!(secure.contains("; Secure"));

        let insecure = build_set_cookie("tok123", expires, false);
        assert!(insecure.starts_with("admin_session=tok123"));
        assert!(!insecure.contains("; Secure"));
    }

    #[test]
    fn extract_cookie_parses_named_cookie_out_of_multiple() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("foo=bar; admin_session=tok123; theme=dark"),
        );

        assert_eq!(extract_cookie(&headers, false), Some("tok123"));
        assert_eq!(extract_cookie(&headers, true), None);
    }
}
