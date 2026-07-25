use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::model::{OAuthState, Provider};

#[derive(Debug, Default, serde::Deserialize)]
pub struct ProviderPatch {
    pub name: Option<String>,
    // Option<Option<T>>: outer None = leave alone, inner None = set NULL.
    pub base_url: Option<Option<String>>,
    pub api_key: Option<Option<String>>,
    pub upstream_model: Option<String>,
}

pub async fn list_providers(db: &SqlitePool) -> Result<Vec<Provider>, AppError> {
    Ok(sqlx::query_as::<_, Provider>("SELECT * FROM providers ORDER BY name")
        .fetch_all(db)
        .await?)
}

pub async fn get_provider(db: &SqlitePool, id: &str) -> Result<Provider, AppError> {
    sqlx::query_as::<_, Provider>("SELECT * FROM providers WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn insert_provider(db: &SqlitePool, p: &Provider) -> Result<(), AppError> {
    let res = sqlx::query(
        "INSERT INTO providers (id,name,wire_format,kind,base_url,api_key,upstream_model,created_at,updated_at)
         VALUES (?,?,?,?,?,?,?,?,?)",
    )
    .bind(&p.id)
    .bind(&p.name)
    .bind(p.wire_format)
    .bind(p.kind)
    .bind(&p.base_url)
    .bind(&p.api_key)
    .bind(&p.upstream_model)
    .bind(p.created_at)
    .bind(p.updated_at)
    .execute(db)
    .await;

    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(AppError::Conflict(format!("provider name '{}' already exists", p.name)))
        }
        Err(e) => Err(AppError::Db(e)),
    }
}

pub async fn update_provider(
    db: &SqlitePool,
    id: &str,
    patch: &ProviderPatch,
) -> Result<Provider, AppError> {
    let mut p = get_provider(db, id).await?;
    if let Some(n) = &patch.name {
        p.name = n.clone();
    }
    if let Some(b) = &patch.base_url {
        p.base_url = b.clone();
    }
    if let Some(k) = &patch.api_key {
        p.api_key = k.clone();
    }
    if let Some(m) = &patch.upstream_model {
        p.upstream_model = m.clone();
    }
    p.updated_at = Utc::now();

    let res = sqlx::query(
        "UPDATE providers SET name=?, base_url=?, api_key=?, upstream_model=?, updated_at=? WHERE id=?",
    )
    .bind(&p.name)
    .bind(&p.base_url)
    .bind(&p.api_key)
    .bind(&p.upstream_model)
    .bind(p.updated_at)
    .bind(id)
    .execute(db)
    .await;

    match res {
        Ok(_) => Ok(p),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(AppError::Conflict("provider name already exists".into()))
        }
        Err(e) => Err(AppError::Db(e)),
    }
}

pub async fn delete_provider(db: &SqlitePool, id: &str) -> Result<(), AppError> {
    let n = sqlx::query("DELETE FROM providers WHERE id = ?")
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

pub async fn get_oauth_state(
    db: &SqlitePool,
    provider_id: &str,
) -> Result<Option<OAuthState>, AppError> {
    Ok(
        sqlx::query_as::<_, OAuthState>("SELECT * FROM provider_oauth_state WHERE provider_id = ?")
            .bind(provider_id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn upsert_oauth_tokens(
    db: &SqlitePool,
    provider_id: &str,
    access: Option<&str>,
    refresh: Option<&str>,
    id_token: Option<&str>,
    access_expires_at: Option<DateTime<Utc>>,
    provider_data: &serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO provider_oauth_state
           (provider_id, access_token, refresh_token, id_token, access_expires_at, provider_data, updated_at)
         VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(provider_id) DO UPDATE SET
           access_token=excluded.access_token,
           refresh_token=excluded.refresh_token,
           id_token=excluded.id_token,
           access_expires_at=excluded.access_expires_at,
           provider_data=excluded.provider_data,
           updated_at=excluded.updated_at",
    )
    .bind(provider_id)
    .bind(access)
    .bind(refresh)
    .bind(id_token)
    .bind(access_expires_at)
    .bind(provider_data.to_string())
    .bind(Utc::now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn store_pkce(
    db: &SqlitePool,
    provider_id: &str,
    verifier: &str,
    state: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO provider_oauth_state (provider_id, pkce_verifier, oauth_state, updated_at)
         VALUES (?,?,?,?)
         ON CONFLICT(provider_id) DO UPDATE SET
           pkce_verifier=excluded.pkce_verifier,
           oauth_state=excluded.oauth_state,
           updated_at=excluded.updated_at",
    )
    .bind(provider_id)
    .bind(verifier)
    .bind(state)
    .bind(Utc::now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn clear_pkce(db: &SqlitePool, provider_id: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE provider_oauth_state SET pkce_verifier=NULL, oauth_state=NULL, updated_at=? WHERE provider_id=?",
    )
    .bind(Utc::now())
    .bind(provider_id)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use chrono::Utc;

    fn sample() -> Provider {
        Provider {
            id: "p1".into(),
            name: "P1".into(),
            wire_format: WireFormat::OpenAi,
            kind: ProviderKind::Passthrough,
            base_url: Some("https://api.example.com".into()),
            api_key: Some("sk-abc".into()),
            upstream_model: "gpt-4o".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn insert_get_update_delete_roundtrip() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();

        let got = get_provider(&db, "p1").await.unwrap();
        assert_eq!(got.name, "P1");

        let patch = ProviderPatch {
            name: Some("P1b".into()),
            base_url: None,
            api_key: Some(Some("sk-new".into())),
            upstream_model: Some("gpt-4o-mini".into()),
        };
        let up = update_provider(&db, "p1", &patch).await.unwrap();
        assert_eq!(up.name, "P1b");
        assert_eq!(up.upstream_model, "gpt-4o-mini");
        assert_eq!(up.api_key.as_deref(), Some("sk-new"));

        delete_provider(&db, "p1").await.unwrap();
        assert!(matches!(
            get_provider(&db, "p1").await,
            Err(crate::core::error::AppError::NotFound)
        ));
    }

    #[tokio::test]
    async fn duplicate_name_is_conflict() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();
        let mut dup = sample();
        dup.id = "p2".into();
        assert!(matches!(
            insert_provider(&db, &dup).await,
            Err(crate::core::error::AppError::Conflict(_))
        ));
    }
}
