use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::model::{Pool, PoolMember};

pub async fn list_pools(db: &SqlitePool) -> Result<Vec<Pool>, AppError> {
    Ok(sqlx::query_as::<_, Pool>("SELECT * FROM pools ORDER BY id")
        .fetch_all(db)
        .await?)
}

pub async fn get_pool(db: &SqlitePool, id: &str) -> Result<Pool, AppError> {
    sqlx::query_as::<_, Pool>("SELECT * FROM pools WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn insert_pool(db: &SqlitePool, p: &Pool) -> Result<(), AppError> {
    let res = sqlx::query(
        "INSERT INTO pools (id, wire_format, created_at, strategy, sticky_limit) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&p.id)
    .bind(p.wire_format)
    .bind(p.created_at)
    .bind(p.strategy)
    .bind(p.sticky_limit)
    .execute(db)
    .await;

    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(AppError::Conflict(
            format!("pool '{}' already exists", p.id),
        )),
        Err(e) => Err(AppError::Db(e)),
    }
}

/// Update an existing pool's selection strategy/sticky_limit. There is no
/// "update wire_format" path - a pool's wire format is fixed at creation,
/// same as `Provider::wire_format` for OAuth kinds.
pub async fn update_pool_strategy(
    db: &SqlitePool,
    id: &str,
    strategy: crate::core::model::PoolStrategy,
    sticky_limit: Option<i64>,
) -> Result<(), AppError> {
    let n = sqlx::query("UPDATE pools SET strategy = ?, sticky_limit = ? WHERE id = ?")
        .bind(strategy)
        .bind(sticky_limit)
        .bind(id)
        .execute(db)
        .await?
        .rows_affected();

    if n == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

pub async fn delete_pool(db: &SqlitePool, id: &str) -> Result<(), AppError> {
    let n = sqlx::query("DELETE FROM pools WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected();

    if n == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

pub async fn list_members(db: &SqlitePool, pool_id: &str) -> Result<Vec<PoolMember>, AppError> {
    Ok(sqlx::query_as::<_, PoolMember>(
        "SELECT pool_id, provider_id, priority, model_override, dataset_logging_override FROM pool_members
         WHERE pool_id = ? ORDER BY priority ASC, provider_id ASC, COALESCE(model_override, '') ASC",
    )
    .bind(pool_id)
    .fetch_all(db)
    .await?)
}

/// A member's real identity is `(pool_id, provider_id, model_override)`
/// (enforced by the `idx_pool_members_identity` unique expression index
/// added in `0005_pool_member_model_identity.sql`) - `model_override ==
/// None` means "inherit `provider.upstream_model`", and `''` collapses to
/// that same slot under the index's `COALESCE(model_override, '')`. A
/// client-supplied empty string must never be stored literally (it would
/// silently collide with a real no-override row, or worse, survive as its
/// own distinct-looking-but-empty upstream model) - normalize it to `None`
/// here rather than trusting every caller to have done so already.
pub async fn upsert_member(db: &SqlitePool, m: &PoolMember) -> Result<(), AppError> {
    let model_override = m.model_override.as_deref().filter(|s| !s.is_empty());
    let res = sqlx::query(
        "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES (?, ?, ?, ?)
         ON CONFLICT (pool_id, provider_id, COALESCE(model_override, '')) DO UPDATE SET priority = excluded.priority",
    )
    .bind(&m.pool_id)
    .bind(&m.provider_id)
    .bind(m.priority)
    .bind(model_override)
    .execute(db)
    .await;

    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => Err(AppError::BadRequest(
            "unknown pool_id or provider_id".into(),
        )),
        Err(e) => Err(AppError::Db(e)),
    }
}

/// `model` selects one specific member by its full identity
/// `(pool_id, provider_id, model_override)` - `Some("")` means "the member
/// with no override" (`model_override IS NULL`), matching how `''`
/// collapses to `NULL` under `idx_pool_members_identity`. `None` preserves
/// the pre-fix behavior: delete every member for that provider in the
/// pool, regardless of model (a no-op distinction when there's only ever
/// been one, which is every caller that predates this).
pub async fn delete_member(
    db: &SqlitePool,
    pool_id: &str,
    provider_id: &str,
    model: Option<&str>,
) -> Result<(), AppError> {
    let n = match model {
        Some(m) => {
            sqlx::query(
                "DELETE FROM pool_members WHERE pool_id = ? AND provider_id = ? AND COALESCE(model_override, '') = ?",
            )
            .bind(pool_id)
            .bind(provider_id)
            .bind(m)
            .execute(db)
            .await?
            .rows_affected()
        }
        None => {
            sqlx::query("DELETE FROM pool_members WHERE pool_id = ? AND provider_id = ?")
                .bind(pool_id)
                .bind(provider_id)
                .execute(db)
                .await?
                .rows_affected()
        }
    };

    if n == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use crate::core::model::{Pool, PoolMember, WireFormat};
    use chrono::Utc;

    async fn seed_provider(db: &sqlx::SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO providers
                (id, name, wire_format, kind, base_url, api_key, upstream_model, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(id)
        .bind(WireFormat::OpenAi)
        .bind("passthrough")
        .bind("u")
        .bind("k")
        .bind("m")
        .bind(Utc::now())
        .bind(Utc::now())
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pool_and_member_crud() {
        let db = init_pool(":memory:").await.unwrap();
        seed_provider(&db, "p1").await;

        insert_pool(
            &db,
            &Pool {
                id: "gpt-4o".into(),
                wire_format: WireFormat::OpenAi,
                created_at: Utc::now(),
                strategy: Default::default(),
                sticky_limit: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(list_pools(&db).await.unwrap().len(), 1);

        upsert_member(
            &db,
            &PoolMember {
                pool_id: "gpt-4o".into(),
                provider_id: "p1".into(),
                priority: 5,
                model_override: None,
                dataset_logging_override: None,
            },
        )
        .await
        .unwrap();
        let members = list_members(&db, "gpt-4o").await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].priority, 5);

        // Same identity (provider p1, no override) - upserting again
        // updates the existing row in place rather than inserting a
        // second one.
        upsert_member(
            &db,
            &PoolMember {
                pool_id: "gpt-4o".into(),
                provider_id: "p1".into(),
                priority: 9,
                model_override: None,
                dataset_logging_override: None,
            },
        )
        .await
        .unwrap();
        let same_identity = list_members(&db, "gpt-4o").await.unwrap();
        assert_eq!(same_identity.len(), 1, "same (provider, model) upsert must update in place, not insert");
        assert_eq!(same_identity[0].priority, 9);

        // Different identity (same provider p1, but a real model_override)
        // - this is the whole point of the fix: p1 can occupy a second
        // slot in the same pool as long as the model differs.
        upsert_member(
            &db,
            &PoolMember {
                pool_id: "gpt-4o".into(),
                provider_id: "p1".into(),
                priority: 1,
                model_override: Some("gpt-5.6-sol".into()),
                dataset_logging_override: None,
            },
        )
        .await
        .unwrap();
        let updated = list_members(&db, "gpt-4o").await.unwrap();
        assert_eq!(updated.len(), 2, "different model_override for the same provider must insert a new member");
        assert_eq!(updated[0].priority, 1);
        assert_eq!(updated[0].model_override.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(updated[1].priority, 9);
        assert_eq!(updated[1].model_override, None);

        // Deleting without a `model` filter removes every member for that
        // provider in the pool - today's behavior, preserved.
        delete_member(&db, "gpt-4o", "p1", None).await.unwrap();
        assert!(list_members(&db, "gpt-4o").await.unwrap().is_empty());

        delete_pool(&db, "gpt-4o").await.unwrap();
        assert!(matches!(
            get_pool(&db, "gpt-4o").await,
            Err(crate::core::error::AppError::NotFound)
        ));
    }
}
