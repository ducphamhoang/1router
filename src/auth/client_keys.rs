//! Per-caller API keys for `/v1/*`.
//!
//! The admin issues one key per user/client; `require_bearer` resolves the
//! presented key to a [`Caller`] and stashes it in the request extensions so
//! the proxy can log who made the request. Only a SHA-256 of the raw key is
//! persisted - the raw key is returned exactly once, from [`create_key`].
//! Upstream provider credentials are unaffected by any of this.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::state::AppState;

/// Prefix on every issued key, so a leaked one is recognisable as a
/// 1router credential (and distinguishable from upstream provider keys).
pub const KEY_PREFIX: &str = "1r_";
/// How many leading characters of the raw key are kept in `key_prefix` for
/// display in the admin UI.
const DISPLAY_PREFIX_LEN: usize = 10;
/// `caller_name` recorded for requests authenticated with the shared secret.
pub const ADMIN_CALLER_NAME: &str = "admin";
const MAX_NAME_LEN: usize = 64;
/// `last_used_at` is only rewritten when it is older than this, so a busy
/// key doesn't turn every request into a DB write.
const LAST_USED_RESOLUTION: ChronoDuration = ChronoDuration::seconds(60);

/// Who made a `/v1/*` request. Inserted into the request extensions by
/// `auth::middleware::require_bearer`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    /// `None` for the shared admin secret and for anonymous open-access calls.
    pub key_id: Option<String>,
    /// `Some("admin")` for the shared secret, `None` for anonymous.
    pub name: Option<String>,
}

impl Caller {
    pub fn admin() -> Self {
        Caller {
            key_id: None,
            name: Some(ADMIN_CALLER_NAME.to_string()),
        }
    }

    pub fn anonymous() -> Self {
        Caller::default()
    }
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct ClientKey {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The one response that ever carries the raw key.
#[derive(Debug, Serialize)]
pub struct CreatedClientKey {
    #[serde(flatten)]
    pub key: ClientKey,
    pub api_key: String,
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

/// The credential a client presented: `Authorization: Bearer <key>` (OpenAI
/// SDKs) or `x-api-key: <key>` (Anthropic SDKs).
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AppError::BadRequest(format!(
            "name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    // Reserved: these are what request logs show for the shared secret and
    // for open-access calls, so a key named after them would be ambiguous.
    if name.eq_ignore_ascii_case(ADMIN_CALLER_NAME) || name.eq_ignore_ascii_case("anonymous") {
        return Err(AppError::BadRequest(format!("name '{name}' is reserved")));
    }
    Ok(name.to_string())
}

pub async fn create_key(db: &SqlitePool, name: &str) -> Result<CreatedClientKey, AppError> {
    let name = validate_name(name)?;
    let raw = generate_raw_key();
    let key = ClientKey {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        key_prefix: raw.chars().take(DISPLAY_PREFIX_LEN).collect(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked_at: None,
    };

    sqlx::query(
        "INSERT INTO client_keys (id, name, key_prefix, key_hash, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&key.id)
    .bind(&key.name)
    .bind(&key.key_prefix)
    .bind(hash_key(&raw))
    .bind(key.created_at)
    .execute(db)
    .await?;

    Ok(CreatedClientKey { key, api_key: raw })
}

pub async fn list_keys(db: &SqlitePool) -> Result<Vec<ClientKey>, AppError> {
    Ok(sqlx::query_as::<_, ClientKey>(
        "SELECT id, name, key_prefix, created_at, last_used_at, revoked_at
         FROM client_keys ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await?)
}

/// Soft revoke: the row stays so historical request logs keep resolving to
/// a name. Revoking an already-revoked key is a no-op, not an error.
pub async fn revoke_key(db: &SqlitePool, id: &str) -> Result<ClientKey, AppError> {
    sqlx::query("UPDATE client_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(Utc::now())
        .bind(id)
        .execute(db)
        .await?;

    sqlx::query_as::<_, ClientKey>(
        "SELECT id, name, key_prefix, created_at, last_used_at, revoked_at
         FROM client_keys WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Resolves a raw key to its (non-revoked) caller. Bumps `last_used_at` in
/// the background, at most once per [`LAST_USED_RESOLUTION`].
pub async fn authenticate(db: &SqlitePool, raw: &str) -> Result<Option<Caller>, AppError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, name FROM client_keys WHERE key_hash = ? AND revoked_at IS NULL",
    )
    .bind(hash_key(raw))
    .fetch_optional(db)
    .await?;

    let Some((id, name)) = row else {
        return Ok(None);
    };

    let db = db.clone();
    let key_id = id.clone();
    tokio::spawn(async move {
        let now = Utc::now();
        let _ = sqlx::query(
            "UPDATE client_keys SET last_used_at = ?
             WHERE id = ? AND (last_used_at IS NULL OR last_used_at < ?)",
        )
        .bind(now)
        .bind(&key_id)
        .bind(now - LAST_USED_RESOLUTION)
        .execute(&db)
        .await
        .map_err(|e| tracing::warn!(error = %e, "client_keys: last_used_at update failed"));
    });

    Ok(Some(Caller {
        key_id: Some(id),
        name: Some(name),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/client-keys", get(list).post(create))
        .route("/admin/client-keys/:id", delete(revoke))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
}

async fn list(State(s): State<AppState>) -> Result<Json<Vec<ClientKey>>, AppError> {
    Ok(Json(list_keys(&s.db).await?))
}

async fn create(
    State(s): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<(StatusCode, Json<CreatedClientKey>), AppError> {
    Ok((StatusCode::CREATED, Json(create_key(&s.db, &body.name).await?)))
}

async fn revoke(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ClientKey>, AppError> {
    Ok(Json(revoke_key(&s.db, &id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;

    #[tokio::test]
    async fn created_key_authenticates_until_revoked() {
        let db = init_pool(":memory:").await.unwrap();
        let created = create_key(&db, "  alice ").await.unwrap();
        assert_eq!(created.key.name, "alice");
        assert!(created.api_key.starts_with(KEY_PREFIX));
        assert!(created.api_key.starts_with(&created.key.key_prefix));

        let caller = authenticate(&db, &created.api_key).await.unwrap().unwrap();
        assert_eq!(caller.name.as_deref(), Some("alice"));
        assert_eq!(caller.key_id.as_deref(), Some(created.key.id.as_str()));

        let revoked = revoke_key(&db, &created.key.id).await.unwrap();
        assert!(revoked.revoked_at.is_some());
        assert!(authenticate(&db, &created.api_key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unknown_key_does_not_authenticate() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(authenticate(&db, "1r_nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn raw_key_is_never_stored() {
        let db = init_pool(":memory:").await.unwrap();
        let created = create_key(&db, "bob").await.unwrap();
        let stored: (String,) = sqlx::query_as("SELECT key_hash FROM client_keys")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_ne!(stored.0, created.api_key);
        assert_eq!(stored.0, hash_key(&created.api_key));
    }

    #[tokio::test]
    async fn rejects_empty_and_reserved_names() {
        let db = init_pool(":memory:").await.unwrap();
        for bad in ["", "   ", "admin", "Anonymous"] {
            assert!(
                matches!(create_key(&db, bad).await, Err(AppError::BadRequest(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn revoke_unknown_id_is_not_found() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(matches!(
            revoke_key(&db, "missing").await,
            Err(AppError::NotFound)
        ));
    }

    #[test]
    fn presented_key_reads_bearer_then_x_api_key() {
        let mut h = HeaderMap::new();
        assert_eq!(presented_key(&h), None);
        h.insert("x-api-key", "k2".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k2"));
        h.insert("authorization", "Bearer k1".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k1"));
    }
}
