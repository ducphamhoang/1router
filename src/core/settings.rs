use sqlx::SqlitePool;

/// Read a boolean setting from the existing server_secrets key/value table.
/// Only the two values written by `set_bool` are accepted; a corrupted or
/// hand-edited value must fail closed rather than being guessed.
pub async fn get_bool(db: &SqlitePool, name: &str) -> anyhow::Result<Option<bool>> {
    let value = sqlx::query_scalar::<_, String>("SELECT value FROM server_secrets WHERE name = ?")
        .bind(name)
        .fetch_optional(db)
        .await?;

    value
        .map(|value| match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => anyhow::bail!("invalid boolean setting {name:?}: {other:?}"),
        })
        .transpose()
}

/// Store a boolean setting, replacing an existing value for the same name.
pub async fn set_bool(db: &SqlitePool, name: &str, value: bool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO server_secrets (name, value) VALUES (?, ?)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value",
    )
    .bind(name)
    .bind(if value { "true" } else { "false" })
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::core::db::init_pool;

    #[tokio::test]
    async fn empty_table_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_pool(dir.path().join("settings.db").to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            super::get_bool(&db, "require_shared_secret").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn set_and_get_round_trip_true_and_false() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_pool(dir.path().join("settings.db").to_str().unwrap())
            .await
            .unwrap();

        super::set_bool(&db, "require_shared_secret", true)
            .await
            .unwrap();
        assert_eq!(
            super::get_bool(&db, "require_shared_secret").await.unwrap(),
            Some(true)
        );
        super::set_bool(&db, "require_shared_secret", false)
            .await
            .unwrap();
        assert_eq!(
            super::get_bool(&db, "require_shared_secret").await.unwrap(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn garbage_value_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_pool(dir.path().join("settings.db").to_str().unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO server_secrets (name, value) VALUES (?, ?)")
            .bind("require_shared_secret")
            .bind("maybe")
            .execute(&db)
            .await
            .unwrap();

        assert!(super::get_bool(&db, "require_shared_secret").await.is_err());
    }

    #[tokio::test]
    async fn set_updates_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_pool(dir.path().join("settings.db").to_str().unwrap())
            .await
            .unwrap();
        super::set_bool(&db, "require_shared_secret", true)
            .await
            .unwrap();
        super::set_bool(&db, "require_shared_secret", false)
            .await
            .unwrap();
        assert_eq!(
            super::get_bool(&db, "require_shared_secret").await.unwrap(),
            Some(false)
        );
    }
}
