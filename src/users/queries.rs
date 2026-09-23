use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::core::error::{validate_path_id, AppError};
use crate::users::{Caller, ADMIN_USER_ID};

/// Prefix on every issued key, so a leaked one is recognisable as a 1router
/// credential (and distinguishable from upstream provider keys). No parsing
/// significance - validation hashes the whole string.
pub const KEY_PREFIX: &str = "1r_";
/// How many leading characters of the raw key are kept in `key_prefix` for
/// display in the admin UI.
const DISPLAY_PREFIX_LEN: usize = 10;
const MAX_NAME_LEN: usize = 64;
/// `last_used_at` is only rewritten when it is older than this, so a busy
/// key doesn't turn every request into a DB write.
const LAST_USED_RESOLUTION: ChronoDuration = ChronoDuration::seconds(60);

const USER_COLUMNS: &str = "id, name, key_prefix, created_at, last_used_at, revoked_at";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The only response shape that ever carries the raw key (create, rotate).
#[derive(Debug, Serialize)]
pub struct UserWithKey {
    #[serde(flatten)]
    pub user: User,
    pub api_key: String,
}

/// Export/import shape: includes `key_hash` (irreversible, and the export
/// already carries real provider `api_key`s) so a restore brings back
/// working keys, never the raw key.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserExport {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
}

fn hash_key(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn generate_raw_key() -> String {
    use rand::RngCore;

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    format!("{KEY_PREFIX}{hex}")
}

fn display_prefix(raw: &str) -> String {
    raw.chars().take(DISPLAY_PREFIX_LEN).collect()
}

fn validate_id(id: &str) -> Result<(), AppError> {
    validate_path_id(id)?;
    if id != id.trim() {
        return Err(AppError::BadRequest(
            "id must not have leading/trailing whitespace".into(),
        ));
    }
    // Reserved: this is what request logs show for the shared secret, and
    // what "anonymous" would naturally be read as.
    if id.eq_ignore_ascii_case(ADMIN_USER_ID) || id.eq_ignore_ascii_case("anonymous") {
        return Err(AppError::BadRequest(format!("id '{id}' is reserved")));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AppError::BadRequest(format!(
            "name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    Ok(name.to_string())
}

pub async fn get_user(db: &SqlitePool, id: &str) -> Result<User, AppError> {
    sqlx::query_as::<_, User>(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

/// `name` is a free-form display label; blank falls back to the id.
pub async fn create_user(
    db: &SqlitePool,
    id: &str,
    name: Option<&str>,
) -> Result<UserWithKey, AppError> {
    validate_id(id)?;
    let name = validate_name(name.unwrap_or(""))?;
    let raw = generate_raw_key();
    let user = User {
        id: id.to_string(),
        name: if name.is_empty() { id.to_string() } else { name },
        key_prefix: display_prefix(&raw),
        created_at: Utc::now(),
        last_used_at: None,
        revoked_at: None,
    };

    let res = sqlx::query(
        "INSERT INTO users (id, name, key_prefix, key_hash, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&user.id)
    .bind(&user.name)
    .bind(&user.key_prefix)
    .bind(hash_key(&raw))
    .bind(user.created_at)
    .execute(db)
    .await;
    match res {
        Ok(_) => Ok(UserWithKey { user, api_key: raw }),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(AppError::Conflict(
            format!("user '{id}' already exists"),
        )),
        Err(e) => Err(e.into()),
    }
}

pub async fn list_users(db: &SqlitePool) -> Result<Vec<User>, AppError> {
    Ok(sqlx::query_as::<_, User>(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY created_at DESC"
    ))
    .fetch_all(db)
    .await?)
}

/// Soft revoke. Idempotent: revoking an already-revoked user is a no-op.
pub async fn revoke_user(db: &SqlitePool, id: &str) -> Result<User, AppError> {
    sqlx::query("UPDATE users SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(Utc::now())
        .bind(id)
        .execute(db)
        .await?;
    get_user(db, id).await
}

/// Issues a replacement key in place (same id/name/created_at); the old key
/// stops working immediately. A revoked user stays revoked - rotating one
/// is a 409, not a silent reactivation.
pub async fn rotate_user_key(db: &SqlitePool, id: &str) -> Result<UserWithKey, AppError> {
    let current = get_user(db, id).await?;
    if current.revoked_at.is_some() {
        return Err(AppError::Conflict(format!("user '{id}' is revoked")));
    }
    let raw = generate_raw_key();
    let prefix = display_prefix(&raw);
    sqlx::query(
        "UPDATE users SET key_hash = ?, key_prefix = ?, last_used_at = NULL
         WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(hash_key(&raw))
    .bind(&prefix)
    .bind(id)
    .execute(db)
    .await?;
    Ok(UserWithKey {
        user: get_user(db, id).await?,
        api_key: raw,
    })
}

/// Resolves a raw key to its (non-revoked) user. Bumps `last_used_at` in the
/// background, at most once per [`LAST_USED_RESOLUTION`].
pub async fn authenticate(db: &SqlitePool, raw: &str) -> Result<Option<Caller>, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE key_hash = ? AND revoked_at IS NULL")
            .bind(hash_key(raw))
            .fetch_optional(db)
            .await?;

    let Some((id,)) = row else {
        return Ok(None);
    };

    let db = db.clone();
    let user_id = id.clone();
    tokio::spawn(async move {
        let now = Utc::now();
        let _ = sqlx::query(
            "UPDATE users SET last_used_at = ?
             WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
        )
        .bind(now)
        .bind(&user_id)
        .bind(now - LAST_USED_RESOLUTION)
        .execute(&db)
        .await
        .map_err(|e| tracing::warn!(error = %e, "users: last_used_at update failed"));
    });

    Ok(Some(Caller { user_id: Some(id) }))
}

pub async fn export_users(db: &SqlitePool) -> Result<Vec<UserExport>, AppError> {
    Ok(sqlx::query_as::<_, UserExport>(
        "SELECT id, name, key_prefix, key_hash, created_at, last_used_at, revoked_at
         FROM users ORDER BY id",
    )
    .fetch_all(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;

    #[tokio::test]
    async fn created_key_authenticates_until_revoked() {
        let db = init_pool(":memory:").await.unwrap();
        let created = create_user(&db, "alice", Some("  Alice N ")).await.unwrap();
        assert_eq!(created.user.name, "Alice N");
        assert!(created.api_key.starts_with(KEY_PREFIX));
        assert!(created.api_key.starts_with(&created.user.key_prefix));

        let caller = authenticate(&db, &created.api_key).await.unwrap().unwrap();
        assert_eq!(caller.user_id.as_deref(), Some("alice"));

        let revoked = revoke_user(&db, "alice").await.unwrap();
        assert!(revoked.revoked_at.is_some());
        assert!(authenticate(&db, &created.api_key).await.unwrap().is_none());
        // idempotent
        let again = revoke_user(&db, "alice").await.unwrap();
        assert_eq!(again.revoked_at, revoked.revoked_at);
    }

    #[tokio::test]
    async fn blank_name_falls_back_to_id() {
        let db = init_pool(":memory:").await.unwrap();
        assert_eq!(create_user(&db, "bob", None).await.unwrap().user.name, "bob");
        assert_eq!(create_user(&db, "bo2", Some(" ")).await.unwrap().user.name, "bo2");
    }

    #[tokio::test]
    async fn rotate_replaces_the_key_in_place() {
        let db = init_pool(":memory:").await.unwrap();
        let created = create_user(&db, "carol", None).await.unwrap();
        let rotated = rotate_user_key(&db, "carol").await.unwrap();
        assert_ne!(rotated.api_key, created.api_key);
        assert_eq!(rotated.user.created_at, created.user.created_at);
        assert!(authenticate(&db, &created.api_key).await.unwrap().is_none());
        assert!(authenticate(&db, &rotated.api_key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rotating_a_revoked_user_is_a_conflict() {
        let db = init_pool(":memory:").await.unwrap();
        create_user(&db, "dave", None).await.unwrap();
        revoke_user(&db, "dave").await.unwrap();
        assert!(matches!(
            rotate_user_key(&db, "dave").await,
            Err(AppError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn duplicate_id_is_a_conflict() {
        let db = init_pool(":memory:").await.unwrap();
        create_user(&db, "erin", None).await.unwrap();
        assert!(matches!(
            create_user(&db, "erin", None).await,
            Err(AppError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn raw_key_is_never_stored() {
        let db = init_pool(":memory:").await.unwrap();
        let created = create_user(&db, "frank", None).await.unwrap();
        let stored = export_users(&db).await.unwrap();
        assert_ne!(stored[0].key_hash, created.api_key);
        assert_eq!(stored[0].key_hash, hash_key(&created.api_key));
    }

    #[tokio::test]
    async fn rejects_bad_and_reserved_ids() {
        let db = init_pool(":memory:").await.unwrap();
        for bad in ["", "   ", "a/b", " pad", "admin", "Anonymous"] {
            assert!(
                matches!(create_user(&db, bad, None).await, Err(AppError::BadRequest(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn unknown_ids_are_not_found() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(matches!(revoke_user(&db, "x").await, Err(AppError::NotFound)));
        assert!(matches!(rotate_user_key(&db, "x").await, Err(AppError::NotFound)));
        assert!(authenticate(&db, "1r_nope").await.unwrap().is_none());
    }
}
