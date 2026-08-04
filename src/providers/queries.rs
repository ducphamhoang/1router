use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::model::{OAuthState, Provider, ProviderKind, WireFormat};

#[derive(Debug, Default, serde::Deserialize)]
pub struct ProviderPatch {
    pub name: Option<String>,
    // Option<Option<T>>: outer None = leave alone, inner None = set NULL.
    pub base_url: Option<Option<String>>,
    pub api_key: Option<Option<String>>,
    pub upstream_model: Option<String>,
    // Lets an existing oauth_codex provider switch which client-facing route
    // it serves (openai <-> anthropic) without redoing the OAuth flow - the
    // credentials in `oauth_state` are keyed by provider id, not wire_format.
    pub wire_format: Option<WireFormat>,
}

pub async fn list_providers(db: &SqlitePool) -> Result<Vec<Provider>, AppError> {
    Ok(
        sqlx::query_as::<_, Provider>("SELECT * FROM providers ORDER BY name")
            .fetch_all(db)
            .await?,
    )
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
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(AppError::Conflict(
            format!("provider name '{}' already exists", p.name),
        )),
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
    if let Some(w) = patch.wire_format {
        if w != p.wire_format
            && !matches!(
                p.kind,
                ProviderKind::OauthCodex | ProviderKind::OauthCommandCode
            )
        {
            // Pools must stay homogeneous in wire_format (enforced when a
            // member is added) - reject a flip that would silently strand
            // this provider in a pool speaking the other format. OAuth
            // credentials live in provider_oauth_state keyed by provider id,
            // not wire_format, so flipping either OAuth kind strands nothing.
            let mismatched: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pool_members pm
                 JOIN pools ON pools.id = pm.pool_id
                 WHERE pm.provider_id = ? AND pools.wire_format != ?",
            )
            .bind(id)
            .bind(w)
            .fetch_one(db)
            .await?;
            if mismatched > 0 {
                return Err(AppError::BadRequest(format!(
                    "provider '{id}' is a member of {mismatched} pool(s) that don't speak \
                     wire_format '{w:?}' - remove it from those pools first"
                )));
            }
        }
        p.wire_format = w;
    }
    p.updated_at = Utc::now();

    let res = sqlx::query(
        "UPDATE providers SET name=?, base_url=?, api_key=?, upstream_model=?, wire_format=?, updated_at=? WHERE id=?",
    )
    .bind(&p.name)
    .bind(&p.base_url)
    .bind(&p.api_key)
    .bind(&p.upstream_model)
    .bind(p.wire_format)
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

/// Whether a usable credential is on file for an OAuth-kind provider
/// (Codex's access token, or Command Code's API key, stashed as
/// `access_token` in `provider_oauth_state` either way). Passthrough
/// providers carry their key on the `providers.api_key` column instead and
/// don't need this - callers check `Provider.api_key` directly for those.
pub async fn oauth_credential_configured(db: &SqlitePool, provider_id: &str) -> Result<bool, AppError> {
    Ok(get_oauth_state(db, provider_id)
        .await?
        .and_then(|s| s.access_token)
        .is_some())
}

/// Batch form of [`oauth_credential_configured`], for `GET /admin/providers`
/// listing every provider at once instead of one `provider_oauth_state`
/// lookup per row.
pub async fn oauth_configured_provider_ids(
    db: &SqlitePool,
) -> Result<std::collections::HashSet<String>, AppError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT provider_id FROM provider_oauth_state WHERE access_token IS NOT NULL",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .collect())
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
            wire_format: None,
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

    #[tokio::test]
    async fn wire_format_can_be_flipped_when_not_in_any_pool() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();

        let patch = ProviderPatch {
            wire_format: Some(WireFormat::Anthropic),
            ..Default::default()
        };
        let up = update_provider(&db, "p1", &patch).await.unwrap();
        assert_eq!(up.wire_format, WireFormat::Anthropic);
    }

    #[tokio::test]
    async fn wire_format_flip_is_rejected_while_in_a_mismatched_pool() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();
        sqlx::query(
            "INSERT INTO pools (id, wire_format, created_at) VALUES ('pool1', 'openai', ?)",
        )
        .bind(Utc::now())
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority) VALUES ('pool1', 'p1', 0)",
        )
        .execute(&db)
        .await
        .unwrap();

        let patch = ProviderPatch {
            wire_format: Some(WireFormat::Anthropic),
            ..Default::default()
        };
        assert!(matches!(
            update_provider(&db, "p1", &patch).await,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
        // rejected - the provider's wire_format is unchanged
        assert_eq!(
            get_provider(&db, "p1").await.unwrap().wire_format,
            WireFormat::OpenAi
        );
    }

    #[tokio::test]
    async fn codex_wire_format_flip_is_allowed_while_in_a_mismatched_pool() {
        let db = init_pool(":memory:").await.unwrap();
        let mut codex = sample();
        codex.id = "cx".into();
        codex.name = "Codex".into();
        codex.kind = ProviderKind::OauthCodex;
        insert_provider(&db, &codex).await.unwrap();
        sqlx::query(
            "INSERT INTO pools (id, wire_format, created_at) VALUES ('pool1', 'openai', ?)",
        )
        .bind(Utc::now())
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority) VALUES ('pool1', 'cx', 0)",
        )
        .execute(&db)
        .await
        .unwrap();

        let patch = ProviderPatch {
            wire_format: Some(WireFormat::Anthropic),
            ..Default::default()
        };
        let updated = update_provider(&db, "cx", &patch).await.unwrap();
        assert_eq!(updated.wire_format, WireFormat::Anthropic);
    }

    #[tokio::test]
    async fn commandcode_wire_format_flip_is_allowed_while_in_a_mismatched_pool() {
        let db = init_pool(":memory:").await.unwrap();
        let mut command_code = sample();
        command_code.id = "cc".into();
        command_code.name = "Command Code".into();
        command_code.kind = ProviderKind::OauthCommandCode;
        insert_provider(&db, &command_code).await.unwrap();
        sqlx::query(
            "INSERT INTO pools (id, wire_format, created_at) VALUES ('pool1', 'openai', ?)",
        )
        .bind(Utc::now())
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority) VALUES ('pool1', 'cc', 0)",
        )
        .execute(&db)
        .await
        .unwrap();

        let patch = ProviderPatch {
            wire_format: Some(WireFormat::Anthropic),
            ..Default::default()
        };
        let updated = update_provider(&db, "cc", &patch).await.unwrap();
        assert_eq!(updated.wire_format, WireFormat::Anthropic);
    }
}
