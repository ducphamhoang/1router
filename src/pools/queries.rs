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
    let res = sqlx::query("INSERT INTO pools (id, wire_format, created_at) VALUES (?, ?, ?)")
        .bind(&p.id)
        .bind(p.wire_format)
        .bind(p.created_at)
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
        "SELECT pool_id, provider_id, priority, model_override FROM pool_members WHERE pool_id = ? ORDER BY priority ASC",
    )
    .bind(pool_id)
    .fetch_all(db)
    .await?)
}

pub async fn upsert_member(db: &SqlitePool, m: &PoolMember) -> Result<(), AppError> {
    let res = sqlx::query(
        "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES (?, ?, ?, ?)
         ON CONFLICT(pool_id, provider_id) DO UPDATE SET priority = excluded.priority, model_override = excluded.model_override",
    )
    .bind(&m.pool_id)
    .bind(&m.provider_id)
    .bind(m.priority)
    .bind(&m.model_override)
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

pub async fn delete_member(
    db: &SqlitePool,
    pool_id: &str,
    provider_id: &str,
) -> Result<(), AppError> {
    let n = sqlx::query("DELETE FROM pool_members WHERE pool_id = ? AND provider_id = ?")
        .bind(pool_id)
        .bind(provider_id)
        .execute(db)
        .await?
        .rows_affected();

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
            },
        )
        .await
        .unwrap();
        let members = list_members(&db, "gpt-4o").await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].priority, 5);

        upsert_member(
            &db,
            &PoolMember {
                pool_id: "gpt-4o".into(),
                provider_id: "p1".into(),
                priority: 1,
                model_override: Some("gpt-5.6-sol".into()),
            },
        )
        .await
        .unwrap();
        let updated = list_members(&db, "gpt-4o").await.unwrap();
        assert_eq!(updated[0].priority, 1);
        assert_eq!(updated[0].model_override.as_deref(), Some("gpt-5.6-sol"));

        delete_member(&db, "gpt-4o", "p1").await.unwrap();
        assert!(list_members(&db, "gpt-4o").await.unwrap().is_empty());

        delete_pool(&db, "gpt-4o").await.unwrap();
        assert!(matches!(
            get_pool(&db, "gpt-4o").await,
            Err(crate::core::error::AppError::NotFound)
        ));
    }
}
