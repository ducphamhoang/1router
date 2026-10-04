use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::model::{EffortLevel, OAuthState, Provider, ProviderKind, WireFormat};
use crate::core::reasoning::capability_for;

/// Reject a provider row whose `default_reasoning_effort` its resulting
/// (kind, wire_format, upstream_model) combination can't actually carry.
///
/// Validates the **resulting row**, not the incoming delta: a
/// `PATCH {"upstream_model": "..."}` that leaves an already-set
/// `default_reasoning_effort` stale must be rejected too, not just a patch
/// that sets the effort itself.
///
/// Best-effort UX only - config import (`admin::import_config`, raw SQL) and
/// the onboarding wizard bypass this entirely, so every adapter re-runs
/// `capability_for` at request-build time and silently skips injection if it
/// comes back `Unsupported`. That request-time check, not this one, is the
/// actual correctness guarantee.
fn check_provider_reasoning_effort(p: &Provider) -> Result<(), AppError> {
    let Some(level) = p.default_reasoning_effort else {
        return Ok(());
    };
    if capability_for(p.kind, p.wire_format, &p.upstream_model).supports() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "provider '{}' ({:?} / {:?} / model '{}') does not accept a reasoning effort - \
         clear default_reasoning_effort (currently {:?}) or pick a model that does",
        p.id, p.kind, p.wire_format, p.upstream_model, level
    )))
}

/// Cross-table re-validation for a provider edit: every pool member that
/// carries a `reasoning_effort_override` resolves its effective model as
/// `model_override.unwrap_or(provider.upstream_model)` and its shape from
/// the provider's kind/wire_format, so an `upstream_model` (or
/// `wire_format`) edit can strand a member's override even though the
/// member row itself wasn't touched. Reject the provider edit rather than
/// silently leaving an invalid member behind - same shape as the existing
/// `wire_format` pool-membership guard above.
async fn check_member_reasoning_overrides(
    db: &SqlitePool,
    p: &Provider,
) -> Result<(), AppError> {
    let rows: Vec<(String, Option<String>, EffortLevel)> = sqlx::query_as(
        "SELECT pool_id, model_override, reasoning_effort_override FROM pool_members
         WHERE provider_id = ? AND reasoning_effort_override IS NOT NULL",
    )
    .bind(&p.id)
    .fetch_all(db)
    .await?;

    for (pool_id, model_override, level) in rows {
        let effective = model_override.as_deref().unwrap_or(&p.upstream_model);
        if !capability_for(p.kind, p.wire_format, effective).supports() {
            return Err(AppError::BadRequest(format!(
                "pool '{pool_id}' has a member on provider '{}' with reasoning_effort_override \
                 {level:?}, which model '{effective}' would no longer accept after this edit - \
                 clear that member's override first",
                p.id
            )));
        }
    }
    Ok(())
}


#[derive(Debug, Default, serde::Deserialize)]
pub struct ProviderPatch {
    pub name: Option<String>,
    // Option<Option<T>>: outer None = leave alone, inner None = set NULL.
    #[serde(default, deserialize_with = "double_option")]
    pub base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub api_key: Option<Option<String>>,
    pub upstream_model: Option<String>,
    // Lets an existing oauth_codex provider switch which client-facing route
    // it serves (openai <-> anthropic) without redoing the OAuth flow - the
    // credentials in `oauth_state` are keyed by provider id, not wire_format.
    pub wire_format: Option<WireFormat>,
    pub dataset_logging: Option<bool>,
    // Option<Option<T>> like base_url/api_key above: outer None = leave
    // alone, inner None (an explicit JSON `null`) = clear the default back
    // to "don't inject anything". Unlike those two, this one needs
    // `deserialize_with = "double_option"`: plain serde collapses an
    // explicit `null` into the outer `None` ("leave alone"), which would
    // leave the admin UI with no way at all to clear a stale effort.
    #[serde(default, deserialize_with = "double_option")]
    pub default_reasoning_effort: Option<Option<EffortLevel>>,
}

/// Distinguish "field absent" (`None`) from "field present and null"
/// (`Some(None)`) for a `PATCH` field. `#[serde(default)]` supplies the
/// absent case without ever calling this; a present `null` reaches it and
/// gets wrapped.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// Trim; empty -> None.
pub fn clean_opt(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn clean_endpoint(v: Option<String>) -> Option<String> {
    clean_opt(v).map(|s| s.trim_end_matches('/').to_string())
}

/// Derive a valid provider id from a free-form name (used by the CLI wizard,
/// which doubles the typed name as the id): keep it if already valid, else slugify.
pub fn id_from_name(name: &str) -> String {
    let name = name.trim();
    if validate_id(name).is_ok() {
        return name.to_string();
    }
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(64).collect();
    if slug.is_empty() { "provider".to_string() } else { slug }
}

fn validate_id(id: &str) -> Result<(), AppError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "provider id must be 1-64 chars of letters, digits, '.', '_' or '-', starting with a letter or digit".into(),
        ))
    }
}

fn validate_endpoint(url: &Option<String>) -> Result<(), AppError> {
    match url {
        Some(u) if !(u.starts_with("http://") || u.starts_with("https://")) => Err(
            AppError::BadRequest("endpoint URL must start with http:// or https://".into()),
        ),
        _ => Ok(()),
    }
}

/// Normalize + validate a provider about to be inserted.
pub fn normalize_new(p: &mut Provider) -> Result<(), AppError> {
    p.id = p.id.trim().to_string();
    p.name = p.name.trim().to_string();
    p.upstream_model = p.upstream_model.trim().to_string();
    p.base_url = clean_endpoint(p.base_url.take());
    p.api_key = clean_opt(p.api_key.take());
    validate_id(&p.id)?;
    if p.name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    if p.upstream_model.is_empty() {
        return Err(AppError::BadRequest("upstream_model must not be empty".into()));
    }
    validate_endpoint(&p.base_url)
}

/// True when the provider has what it needs to serve traffic, given whether
/// an OAuth access/refresh token is stored. A passthrough provider needs an
/// endpoint (the key is optional: keyless upstreams exist); codex needs a token.
pub fn is_ready(p: &Provider, has_oauth_token: bool) -> bool {
    match p.kind {
        ProviderKind::Passthrough => p.base_url.as_deref().is_some_and(|u| !u.is_empty()),
        ProviderKind::OauthCodex | ProviderKind::OauthCommandCode => has_oauth_token,
    }
}

pub async fn clear_oauth_tokens(db: &SqlitePool, provider_id: &str) -> Result<(), AppError> {
    sqlx::query("DELETE FROM provider_oauth_state WHERE provider_id = ?")
        .bind(provider_id)
        .execute(db)
        .await?;
    Ok(())
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
    check_provider_reasoning_effort(p)?;
    let id_taken: Option<(String,)> = sqlx::query_as("SELECT id FROM providers WHERE id = ?")
        .bind(&p.id)
        .fetch_optional(db)
        .await?;
    if id_taken.is_some() {
        return Err(AppError::Conflict(format!("provider id '{}' already exists", p.id)));
    }
    let res = sqlx::query(
        "INSERT INTO providers (id,name,wire_format,kind,base_url,api_key,upstream_model,dataset_logging,default_reasoning_effort,created_at,updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&p.id)
    .bind(&p.name)
    .bind(p.wire_format)
    .bind(p.kind)
    .bind(&p.base_url)
    .bind(&p.api_key)
    .bind(&p.upstream_model)
    .bind(p.dataset_logging)
    .bind(p.default_reasoning_effort)
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
        p.name = n.trim().to_string();
        if p.name.is_empty() {
            return Err(AppError::BadRequest("name must not be empty".into()));
        }
    }
    // Codex providers authenticate via OAuth; endpoint/key patches are meaningless.
    let is_codex = matches!(p.kind, ProviderKind::OauthCodex);
    if let Some(b) = patch.base_url.as_ref().filter(|_| !is_codex) {
        p.base_url = clean_endpoint(b.clone());
        validate_endpoint(&p.base_url)?;
    }
    if let Some(k) = patch.api_key.as_ref().filter(|_| !is_codex) {
        p.api_key = clean_opt(k.clone());
    }
    if let Some(m) = &patch.upstream_model {
        p.upstream_model = m.trim().to_string();
        if p.upstream_model.is_empty() {
            return Err(AppError::BadRequest("upstream_model must not be empty".into()));
        }
    }
    if let Some(v) = patch.dataset_logging {
        p.dataset_logging = v;
    }
    if let Some(e) = patch.default_reasoning_effort {
        p.default_reasoning_effort = e;
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
            // DISTINCT pm.pool_id, not COUNT(*): a provider can now have
            // several memberships in the same pool (one per model_override,
            // see migrations/0005_pool_member_model_identity.sql), and the
            // error message below counts pools, not memberships.
            let mismatched: i64 = sqlx::query_scalar(
                "SELECT COUNT(DISTINCT pm.pool_id) FROM pool_members pm
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
    // Validate the *resulting* row (and every pool member that inherits
    // from it), not just the incoming delta - see the two helpers' docs.
    check_provider_reasoning_effort(&p)?;
    check_member_reasoning_overrides(db, &p).await?;
    p.updated_at = Utc::now();

    let res = sqlx::query(
        "UPDATE providers SET name=?, base_url=?, api_key=?, upstream_model=?, wire_format=?, dataset_logging=?, default_reasoning_effort=?, updated_at=? WHERE id=?",
    )
    .bind(&p.name)
    .bind(&p.base_url)
    .bind(&p.api_key)
    .bind(&p.upstream_model)
    .bind(p.wire_format)
    .bind(p.dataset_logging)
    .bind(p.default_reasoning_effort)
    .bind(p.updated_at)
    .bind(id)
    .execute(db)
    .await;

    match res {
        Ok(_) => Ok(p),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(AppError::Conflict(format!("provider name '{}' already exists", p.name)))
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
        .is_some_and(|s| s.access_token.is_some() || s.refresh_token.is_some()))
}

/// Batch form of [`oauth_credential_configured`], for `GET /admin/providers`
/// listing every provider at once instead of one `provider_oauth_state`
/// lookup per row.
pub async fn oauth_configured_provider_ids(
    db: &SqlitePool,
) -> Result<std::collections::HashSet<String>, AppError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT provider_id FROM provider_oauth_state WHERE access_token IS NOT NULL OR refresh_token IS NOT NULL",
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
            dataset_logging: false,
            default_reasoning_effort: None,
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
            dataset_logging: None,
            default_reasoning_effort: None,
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

    #[test]
    fn provider_patch_deserializes_from_an_empty_or_partial_json_object() {
        // The admin PATCH endpoint passes the request body straight through
        // to `serde_json` as a `ProviderPatch` - every field must tolerate
        // being entirely absent from the JSON (a partial patch is the
        // normal case, e.g. `{"name": "x"}` alone), not just `null`.
        let empty: ProviderPatch = serde_json::from_str("{}").unwrap();
        assert!(empty.name.is_none());
        assert!(empty.dataset_logging.is_none());

        let partial: ProviderPatch = serde_json::from_str(r#"{"dataset_logging": true}"#).unwrap();
        assert_eq!(partial.dataset_logging, Some(true));
        assert!(partial.name.is_none());
    }

    #[tokio::test]
    async fn insert_and_update_provider_round_trip_dataset_logging() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample();
        p.dataset_logging = true;
        insert_provider(&db, &p).await.unwrap();
        assert!(get_provider(&db, "p1").await.unwrap().dataset_logging);

        let patch = ProviderPatch {
            dataset_logging: Some(false),
            ..Default::default()
        };
        let up = update_provider(&db, "p1", &patch).await.unwrap();
        assert!(!up.dataset_logging);

        // A patch that doesn't mention dataset_logging leaves it unchanged.
        let patch2 = ProviderPatch {
            name: Some("P1c".into()),
            ..Default::default()
        };
        let up2 = update_provider(&db, "p1", &patch2).await.unwrap();
        assert!(!up2.dataset_logging);
    }

    /// Insert a pool + one member for `provider_id`, with the given
    /// model/reasoning overrides, using raw SQL so the member-side write
    /// validation (added alongside this) can't interfere with setting up a
    /// deliberately-stale fixture.
    async fn seed_member(
        db: &sqlx::SqlitePool,
        pool_id: &str,
        provider_id: &str,
        model_override: Option<&str>,
        reasoning: Option<EffortLevel>,
    ) {
        sqlx::query("INSERT OR IGNORE INTO pools (id, wire_format, created_at) VALUES (?, 'openai', ?)")
            .bind(pool_id)
            .bind(Utc::now())
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO pool_members (pool_id, provider_id, priority, model_override, reasoning_effort_override)
             VALUES (?, ?, 0, ?, ?)",
        )
        .bind(pool_id)
        .bind(provider_id)
        .bind(model_override)
        .bind(reasoning)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn insert_and_update_provider_round_trip_default_reasoning_effort() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample();
        p.upstream_model = "gpt-5.1".into();
        p.default_reasoning_effort = Some(EffortLevel::High);
        insert_provider(&db, &p).await.unwrap();
        assert_eq!(
            get_provider(&db, "p1").await.unwrap().default_reasoning_effort,
            Some(EffortLevel::High)
        );

        // A patch that doesn't mention the field leaves it alone.
        let up = update_provider(
            &db,
            "p1",
            &ProviderPatch {
                name: Some("P1b".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(up.default_reasoning_effort, Some(EffortLevel::High));

        // `Some(Some(_))` sets it; `Some(None)` (an explicit JSON null)
        // clears it back to "inject nothing".
        let up = update_provider(
            &db,
            "p1",
            &ProviderPatch {
                default_reasoning_effort: Some(Some(EffortLevel::Low)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(up.default_reasoning_effort, Some(EffortLevel::Low));

        let up = update_provider(
            &db,
            "p1",
            &ProviderPatch {
                default_reasoning_effort: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(up.default_reasoning_effort, None);
        assert_eq!(
            get_provider(&db, "p1").await.unwrap().default_reasoning_effort,
            None
        );
    }

    #[test]
    fn provider_patch_parses_the_three_reasoning_effort_states() {
        let absent: ProviderPatch = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.default_reasoning_effort, None);
        let cleared: ProviderPatch =
            serde_json::from_str(r#"{"default_reasoning_effort": null}"#).unwrap();
        assert_eq!(cleared.default_reasoning_effort, Some(None));
        let set: ProviderPatch =
            serde_json::from_str(r#"{"default_reasoning_effort": "medium"}"#).unwrap();
        assert_eq!(set.default_reasoning_effort, Some(Some(EffortLevel::Medium)));
    }

    #[tokio::test]
    async fn insert_rejects_a_reasoning_effort_the_model_cannot_carry() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample(); // passthrough / openai / "gpt-4o" -> Unsupported
        p.default_reasoning_effort = Some(EffortLevel::High);
        assert!(matches!(
            insert_provider(&db, &p).await,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn update_rejects_a_reasoning_effort_the_model_cannot_carry() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap(); // gpt-4o
        assert!(matches!(
            update_provider(
                &db,
                "p1",
                &ProviderPatch {
                    default_reasoning_effort: Some(Some(EffortLevel::High)),
                    ..Default::default()
                }
            )
            .await,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn update_validates_the_resulting_row_not_just_the_delta() {
        // Set a valid effort first, then edit ONLY upstream_model to
        // something that can't carry it - the patch never mentions
        // default_reasoning_effort, but the resulting row is invalid.
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample();
        p.upstream_model = "gpt-5.1".into();
        p.default_reasoning_effort = Some(EffortLevel::High);
        insert_provider(&db, &p).await.unwrap();

        assert!(matches!(
            update_provider(
                &db,
                "p1",
                &ProviderPatch {
                    upstream_model: Some("gpt-4o".into()),
                    ..Default::default()
                }
            )
            .await,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
        // rejected - nothing was written
        let after = get_provider(&db, "p1").await.unwrap();
        assert_eq!(after.upstream_model, "gpt-5.1");
        assert_eq!(after.default_reasoning_effort, Some(EffortLevel::High));

        // Clearing the effort in the same patch makes the same edit legal.
        let ok = update_provider(
            &db,
            "p1",
            &ProviderPatch {
                upstream_model: Some("gpt-4o".into()),
                default_reasoning_effort: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ok.upstream_model, "gpt-4o");
        assert_eq!(ok.default_reasoning_effort, None);
    }

    #[tokio::test]
    async fn update_rejects_an_upstream_model_edit_that_strands_a_members_override() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample();
        p.upstream_model = "gpt-5.1".into();
        insert_provider(&db, &p).await.unwrap();
        // Member inherits the provider's upstream_model (no model_override)
        // and carries its own reasoning override.
        seed_member(&db, "pool1", "p1", None, Some(EffortLevel::Low)).await;

        assert!(matches!(
            update_provider(
                &db,
                "p1",
                &ProviderPatch {
                    upstream_model: Some("gpt-4o".into()),
                    ..Default::default()
                }
            )
            .await,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
        assert_eq!(
            get_provider(&db, "p1").await.unwrap().upstream_model,
            "gpt-5.1"
        );
    }

    #[tokio::test]
    async fn update_allows_an_upstream_model_edit_when_the_member_pins_its_own_model() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = sample();
        p.upstream_model = "gpt-5.1".into();
        insert_provider(&db, &p).await.unwrap();
        // This member does NOT inherit upstream_model, so the edit can't
        // strand it.
        seed_member(&db, "pool1", "p1", Some("gpt-5.2"), Some(EffortLevel::Low)).await;

        let ok = update_provider(
            &db,
            "p1",
            &ProviderPatch {
                upstream_model: Some("gpt-4o".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ok.upstream_model, "gpt-4o");
    }

    #[test]
    fn patch_null_clears_and_absent_leaves() {
        let p: ProviderPatch = serde_json::from_str(r#"{"api_key": null}"#).unwrap();
        assert_eq!(p.api_key, Some(None));
        assert_eq!(p.base_url, None);
        let p: ProviderPatch = serde_json::from_str(r#"{"api_key": "k"}"#).unwrap();
        assert_eq!(p.api_key, Some(Some("k".into())));
    }

    #[test]
    fn normalize_new_cleans_and_validates() {
        let mut p = sample();
        p.base_url = Some("  https://x.test/v1/chat/completions/ ".into());
        p.api_key = Some("   ".into());
        normalize_new(&mut p).unwrap();
        assert_eq!(p.base_url.as_deref(), Some("https://x.test/v1/chat/completions"));
        assert_eq!(p.api_key, None);

        let mut bad = sample();
        bad.id = "has space".into();
        assert!(matches!(normalize_new(&mut bad), Err(AppError::BadRequest(_))));
        let mut bad = sample();
        bad.id = "".into();
        assert!(normalize_new(&mut bad).is_err());
        let mut bad = sample();
        bad.base_url = Some("ftp://x".into());
        assert!(normalize_new(&mut bad).is_err());
    }

    #[test]
    fn id_from_name_slugifies_only_when_needed() {
        assert_eq!(id_from_name("my-openai"), "my-openai");
        assert_eq!(id_from_name("My OpenAI!"), "my-openai");
        assert_eq!(id_from_name("***"), "provider");
    }

    #[test]
    fn readiness_rules() {
        let mut p = sample();
        assert!(is_ready(&p, false));
        p.base_url = None;
        assert!(!is_ready(&p, false));
        p.kind = ProviderKind::OauthCodex;
        assert!(!is_ready(&p, false));
        assert!(is_ready(&p, true));
    }

    #[tokio::test]
    async fn duplicate_id_and_name_have_distinct_messages() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();
        let mut same_id = sample();
        same_id.name = "Other".into();
        match insert_provider(&db, &same_id).await {
            Err(AppError::Conflict(m)) => assert!(m.contains("id"), "{m}"),
            other => panic!("{other:?}"),
        }
        let mut same_name = sample();
        same_name.id = "p2".into();
        match insert_provider(&db, &same_name).await {
            Err(AppError::Conflict(m)) => assert!(m.contains("name"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn patch_null_api_key_clears_it() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &sample()).await.unwrap();
        let patch: ProviderPatch = serde_json::from_str(r#"{"api_key": null, "base_url": ""}"#).unwrap();
        let up = update_provider(&db, "p1", &patch).await.unwrap();
        assert_eq!(up.api_key, None);
        assert_eq!(up.base_url, None);
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
