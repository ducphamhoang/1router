use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;

pub async fn init_pool(sqlite_path: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(sqlite_path)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_pool_applies_migrations_and_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let pool = init_pool(path.to_str().unwrap()).await.unwrap();

        // journal mode is WAL
        let mode: (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode.0.to_lowercase(), "wal");

        // migrated table exists and is queryable
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n.0, 0);
    }

    /// Exercises the actual `0005` rebuild (`pool_members` -> unique
    /// expression index), not just a post-migration insert - a real
    /// pre-/post-migration test, applying `0001`-`0004` by hand first so
    /// fixture rows exist *before* `0005`'s `INSERT ... SELECT` runs.
    #[tokio::test]
    async fn migration_0005_preserves_pool_members_and_enforces_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let opts = SqliteConnectOptions::from_str(path.to_str().unwrap())
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();

        // Apply 0001-0004 by hand (not sqlx::migrate!, which would also
        // pull in 0005 - we need fixture rows to exist strictly before it).
        for sql in [
            include_str!("../../migrations/0001_init.sql"),
            include_str!("../../migrations/0002_admin_ui.sql"),
            include_str!("../../migrations/0003_pool_member_model_override.sql"),
            include_str!("../../migrations/0004_pool_strategy.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }

        // Fixture: one round_robin pool, two providers, one member with a
        // non-NULL model_override (0003's whole reason for existing) and
        // one with none - both must survive the rebuild unchanged.
        sqlx::raw_sql(
            "INSERT INTO providers (id, name, wire_format, kind, upstream_model, created_at, updated_at)
             VALUES ('p1','p1','openai','passthrough','m1','2026-01-01','2026-01-01'),
                    ('p2','p2','openai','passthrough','m2','2026-01-01','2026-01-01');
             INSERT INTO pools (id, wire_format, created_at, strategy, sticky_limit)
             VALUES ('pool1','openai','2026-01-01','round_robin',2);
             INSERT INTO pool_members (pool_id, provider_id, priority, model_override)
             VALUES ('pool1','p1',1,'override-1'), ('pool1','p2',2,NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();

        let before: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT pool_id, provider_id, priority, model_override FROM pool_members ORDER BY provider_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(before.len(), 2);

        sqlx::raw_sql(include_str!("../../migrations/0005_pool_member_model_identity.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let after: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT pool_id, provider_id, priority, model_override FROM pool_members ORDER BY provider_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(before, after, "0005's rebuild must preserve every row exactly");

        let fk_violations: Vec<(String,)> = sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(fk_violations.is_empty(), "foreign_key_check found violations: {fk_violations:?}");

        let integrity: (String,) = sqlx::query_as("PRAGMA integrity_check")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(integrity.0, "ok");

        // The new unique index must reject a true duplicate identity...
        let dup = sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES ('pool1','p1',9,'override-1')",
        )
        .execute(&pool)
        .await;
        assert!(dup.is_err(), "duplicate (pool_id, provider_id, model_override) must be rejected");

        // ...but accept the same provider with a DIFFERENT model in the same pool -
        // this is the whole point of the fix.
        let ok = sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES ('pool1','p1',9,'override-2')",
        )
        .execute(&pool)
        .await;
        assert!(ok.is_ok(), "same provider + different model must be allowed: {ok:?}");
    }
}
