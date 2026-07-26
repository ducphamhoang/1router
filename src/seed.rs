use sqlx::SqlitePool;

use crate::admin::{import_config, ExportDump};
use crate::core::config::Config;

pub async fn seed_if_configured(db: &SqlitePool, cfg: &Config) -> anyhow::Result<()> {
    let seed_path = match &cfg.seed_path {
        Some(p) => p,
        None => return Ok(()),
    };

    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
        .fetch_one(db)
        .await?;
    if count.0 > 0 {
        tracing::info!("seed skipped: providers table not empty");
        return Ok(());
    }

    let raw = std::fs::read_to_string(seed_path)
        .map_err(|e| anyhow::anyhow!("failed to read seed file {:?}: {e}", seed_path))?;
    let dump: ExportDump =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("invalid seed JSON: {e}"))?;
    import_config(db, &dump)
        .await
        .map_err(|e| anyhow::anyhow!("seed import failed: {e}"))?;
    tracing::info!(
        providers = dump.providers.len(),
        pools = dump.pools.len(),
        "first-boot seed applied"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use std::time::Duration;

    fn cfg_with_seed(path: std::path::PathBuf) -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "s".into(),
            admin_secret: None,
            seed_path: Some(path),
            connect_timeout: Duration::from_secs(1),
            ttfb_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn seeds_empty_db_from_file() {
        let db = init_pool(":memory:").await.unwrap();
        let dump = serde_json::json!({
            "providers": [{
                "id": "p1", "name": "P1", "wire_format": "openai", "kind": "passthrough",
                "base_url": "https://x", "api_key": "k", "upstream_model": "m",
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
            }],
            "pools": [], "members": []
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), dump.to_string()).unwrap();

        seed_if_configured(&db, &cfg_with_seed(file.path().to_path_buf()))
            .await
            .unwrap();

        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n.0, 1);
    }

    #[tokio::test]
    async fn does_not_seed_nonempty_db() {
        let db = init_pool(":memory:").await.unwrap();
        sqlx::query("INSERT INTO providers (id,name,wire_format,kind,upstream_model,created_at,updated_at)
                     VALUES ('x','X','openai','passthrough','m','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")
            .execute(&db).await.unwrap();

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"providers":[],"pools":[],"members":[]}"#).unwrap();
        seed_if_configured(&db, &cfg_with_seed(file.path().to_path_buf()))
            .await
            .unwrap();

        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n.0, 1); // unchanged
    }
}
