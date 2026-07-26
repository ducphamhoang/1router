//! Interactive terminal onboarding wizard.
//!
//! Design: docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md
//!
//! This module contains no business logic of its own: it sequences calls into
//! `providers::queries`, `pools::queries` and
//! `providers::adapter::codex::oauth`, and owns only the prompt UI plus a few
//! pure helpers (which is where all of its unit tests live).

use crate::core::model::PoolMember;

/// Candidate Codex models, in probe order.
///
/// ChatGPT-subscription auth only accepts a backend-specific, account/plan-
/// specific allowlist that is not discoverable from this codebase - the only
/// way to find the right value is to try candidates against a live login.
/// Kept in sync BY HAND with tests/e2e_real_providers.rs::codex_end_to_end_real;
/// if you update one, update the other (see the spec's accepted-risk section).
pub const CANDIDATE_MODELS: [&str; 5] =
    ["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5", "codex-mini-latest"];

/// Placeholder `upstream_model` for a Codex provider whose real model is not
/// known yet (set at create time, and left in place if every probe fails).
pub const PENDING_MODEL: &str = "pending";

/// Priority for a newly added pool member: 1 in a fresh pool, else
/// max(existing) + 1. Deliberately NOT `len + 1`, which would outrank an
/// existing member whose priority is sparse (e.g. [1, 10] -> 3 jumps 10).
pub fn next_priority(existing: &[PoolMember]) -> i64 {
    existing.iter().map(|m| m.priority).max().unwrap_or(0) + 1
}

/// Accept either a full pasted redirect URL or a bare `code=..&state=..`
/// fragment (users paste both; the browser's address bar gives the former).
pub fn parse_code_and_state(input: &str) -> anyhow::Result<(String, String)> {
    let trimmed = input.trim();
    let query = trimmed.split_once('?').map(|(_, q)| q).unwrap_or(trimmed);
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k.trim() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => anyhow::bail!(
            "could not find both `code` and `state` in the pasted input; \
             paste the full redirect URL, or just `code=...&state=...`"
        ),
    }
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Found(String),
    AllFailed(Vec<(String, u16, String)>),
}

/// Try each model in order, stop at the first HTTP 200.
///
/// Generic over the attempt so the control flow is unit-testable with no
/// network and no real provider; the wizard passes a closure that builds a
/// real adapter request (see P5-6).
pub async fn probe_first_success<F, Fut>(models: &[&str], mut attempt: F) -> ProbeOutcome
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(u16, String), String>>,
{
    let mut failures = Vec::new();
    for model in models {
        match attempt(model.to_string()).await {
            Ok((200, _)) => return ProbeOutcome::Found(model.to_string()),
            Ok((status, body)) => failures.push((model.to_string(), status, body)),
            // A transport error is just another failed attempt - keep going,
            // the next model may hit a different backend path.
            Err(e) => failures.push((model.to_string(), 0, e)),
        }
    }
    ProbeOutcome::AllFailed(failures)
}

use crate::core::error::AppError;
use crate::core::model::{Pool, Provider};
use crate::pools::queries as pool_queries;

/// Add `provider` to `pool_id`, creating the pool if needed.
///
/// Deliberately takes `pool_id` rather than prompting for it, so the whole
/// DB-touching part of the pool step is unit testable; the prompt lives in
/// `run_wizard`.
pub async fn assign_to_pool(
    db: &sqlx::SqlitePool,
    pool_id: &str,
    provider: &Provider,
) -> anyhow::Result<i64> {
    match pool_queries::get_pool(db, pool_id).await {
        Ok(_) => {}
        Err(AppError::NotFound) => {
            pool_queries::insert_pool(
                db,
                &Pool {
                    id: pool_id.to_string(),
                    // A pool's wire_format is what clients speak to it; for a
                    // brand-new pool built around one provider, match the
                    // provider so the two can't disagree.
                    wire_format: provider.wire_format,
                    created_at: chrono::Utc::now(),
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to create pool '{pool_id}': {e}"))?;
        }
        Err(e) => return Err(anyhow::anyhow!("failed to look up pool '{pool_id}': {e}")),
    }

    let existing = pool_queries::list_members(db, pool_id)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list members of '{pool_id}': {e}"))?;
    let priority = next_priority(&existing);

    pool_queries::upsert_member(
        db,
        &PoolMember {
            pool_id: pool_id.to_string(),
            provider_id: provider.id.clone(),
            priority,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to add '{}' to '{pool_id}': {e}", provider.id))?;

    Ok(priority)
}

use crate::core::model::{ProviderKind, WireFormat};
use crate::providers::queries as provider_queries;
use dialoguer::{Confirm, Input, Password, Select};

fn theme() -> dialoguer::theme::ColorfulTheme {
    dialoguer::theme::ColorfulTheme::default()
}

pub(crate) fn build_passthrough_row(
    name: &str,
    wire_format: WireFormat,
    base_url: &str,
    api_key: &str,
    upstream_model: &str,
) -> Provider {
    let now = chrono::Utc::now();
    Provider {
        // The spec deliberately doubles the name as the id: one prompt fewer,
        // and the id is what shows up in logs/stats where the name would
        // otherwise be redundant.
        id: name.to_string(),
        name: name.to_string(),
        wire_format,
        kind: ProviderKind::Passthrough,
        base_url: Some(base_url.to_string()),
        api_key: Some(api_key.to_string()),
        upstream_model: upstream_model.to_string(),
        created_at: now,
        updated_at: now,
    }
}

/// Prompt for a passthrough provider and insert it.
pub async fn add_passthrough_provider(db: &sqlx::SqlitePool) -> anyhow::Result<Provider> {
    // dialoguer blocks the calling thread. That is fine here and NOT worth
    // wrapping in spawn_blocking: the wizard runs either before the axum
    // listener exists (first boot) or in a process that never starts one
    // (`1router setup`), so there is no concurrent work for it to starve.
    let name: String = Input::with_theme(&theme())
        .with_prompt("Provider name (also used as its id)")
        .validate_with(|s: &String| -> Result<(), &str> {
            if s.trim().is_empty() { Err("name cannot be empty") } else { Ok(()) }
        })
        .interact_text()?;
    let name = name.trim().to_string();

    let wire_format = match Select::with_theme(&theme())
        .with_prompt("Wire format")
        .items(&["openai", "anthropic"])
        .default(0)
        .interact()?
    {
        0 => WireFormat::OpenAi,
        _ => WireFormat::Anthropic,
    };

    println!(
        "  note: base_url is POSTed as-is - include the full upstream path, \
         e.g. https://api.openai.com/v1/chat/completions"
    );
    let base_url: String = Input::with_theme(&theme())
        .with_prompt("Upstream base_url (full path)")
        .interact_text()?;

    let api_key: String = Password::with_theme(&theme())
        .with_prompt("API key (input hidden)")
        .interact()?;

    let upstream_model: String = Input::with_theme(&theme())
        .with_prompt("Upstream model (the real model name this provider expects)")
        .interact_text()?;

    let p = build_passthrough_row(
        &name,
        wire_format,
        base_url.trim(),
        api_key.trim(),
        upstream_model.trim(),
    );
    provider_queries::insert_provider(db, &p)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create provider '{}': {e}", p.id))?;
    println!("  created provider '{}'", p.id);
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::PoolMember;

    fn member(priority: i64) -> PoolMember {
        PoolMember { pool_id: "p".into(), provider_id: "x".into(), priority }
    }

    #[test]
    fn next_priority_is_one_for_an_empty_pool() {
        assert_eq!(next_priority(&[]), 1);
    }

    #[test]
    fn next_priority_is_max_plus_one_not_len_plus_one() {
        // len+1 would return 3 here and silently outrank the priority-10 member.
        assert_eq!(next_priority(&[member(1), member(10)]), 11);
    }

    #[test]
    fn next_priority_ignores_ordering_of_input() {
        assert_eq!(next_priority(&[member(10), member(1)]), 11);
    }

    #[test]
    fn parses_full_redirect_url() {
        let (c, s) = parse_code_and_state(
            "  http://localhost:1455/auth/callback?code=abc123&state=st-9&scope=openid\n",
        )
        .unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(s, "st-9");
    }

    #[test]
    fn parses_bare_query_fragment() {
        let (c, s) = parse_code_and_state("code=abc123&state=st-9").unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(s, "st-9");
    }

    #[test]
    fn parse_errors_when_code_or_state_missing() {
        assert!(parse_code_and_state("state=only").is_err());
        assert!(parse_code_and_state("code=only").is_err());
        assert!(parse_code_and_state("total garbage").is_err());
    }

    #[tokio::test]
    async fn probe_stops_at_first_success_and_skips_the_rest() {
        let tried = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let t = tried.clone();
        let out = probe_first_success(&["a", "b", "c"], move |m| {
            let t = t.clone();
            async move {
                t.lock().unwrap().push(m.clone());
                if m == "b" { Ok((200, "{}".into())) } else { Ok((400, "nope".into())) }
            }
        })
        .await;

        assert!(matches!(out, ProbeOutcome::Found(ref m) if m == "b"));
        assert_eq!(&*tried.lock().unwrap(), &["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn probe_reports_every_failure_when_none_succeed() {
        let out = probe_first_success(&["a", "b"], |m| async move {
            Ok((404, format!("no {m}")))
        })
        .await;

        match out {
            ProbeOutcome::AllFailed(fs) => {
                assert_eq!(fs.len(), 2);
                assert_eq!(fs[0], ("a".into(), 404, "no a".into()));
                assert_eq!(fs[1], ("b".into(), 404, "no b".into()));
            }
            ProbeOutcome::Found(m) => panic!("unexpected success: {m}"),
        }
    }

    #[tokio::test]
    async fn probe_treats_transport_error_as_a_failed_attempt_and_continues() {
        let out = probe_first_success(&["a", "b"], |m| async move {
            if m == "a" { Err("connection reset".into()) } else { Ok((200, "{}".into())) }
        })
        .await;
        assert!(matches!(out, ProbeOutcome::Found(ref m) if m == "b"));
    }

    #[test]
    fn candidate_list_matches_the_e2e_test() {
        // If this list changes, tests/e2e_real_providers.rs must change too -
        // the spec calls them out as a pair that goes stale together.
        assert_eq!(
            CANDIDATE_MODELS,
            ["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5", "codex-mini-latest"]
        );
    }

    use crate::core::db::init_pool;
    use crate::core::model::{Provider, ProviderKind, WireFormat};
    use crate::providers::queries::insert_provider;
    use chrono::Utc;

    fn provider(id: &str, wf: WireFormat) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            wire_format: wf,
            kind: ProviderKind::Passthrough,
            base_url: Some("https://x/v1/chat/completions".into()),
            api_key: Some("k".into()),
            upstream_model: "m".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn assign_creates_the_pool_and_uses_priority_one() {
        let db = init_pool(":memory:").await.unwrap();
        let p = provider("p1", WireFormat::OpenAi);
        insert_provider(&db, &p).await.unwrap();

        let prio = assign_to_pool(&db, "my-pool", &p).await.unwrap();
        assert_eq!(prio, 1);

        let pool = crate::pools::queries::get_pool(&db, "my-pool").await.unwrap();
        assert_eq!(pool.wire_format, WireFormat::OpenAi);
        let members = crate::pools::queries::list_members(&db, "my-pool").await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].provider_id, "p1");
        assert_eq!(members[0].priority, 1);
    }

    #[tokio::test]
    async fn assign_inherits_the_providers_wire_format_for_a_new_pool() {
        let db = init_pool(":memory:").await.unwrap();
        let p = provider("p1", WireFormat::Anthropic);
        insert_provider(&db, &p).await.unwrap();
        assign_to_pool(&db, "anth-pool", &p).await.unwrap();
        assert_eq!(
            crate::pools::queries::get_pool(&db, "anth-pool").await.unwrap().wire_format,
            WireFormat::Anthropic
        );
    }

    #[tokio::test]
    async fn assign_appends_behind_existing_members() {
        let db = init_pool(":memory:").await.unwrap();
        let first = provider("p1", WireFormat::OpenAi);
        let second = provider("p2", WireFormat::OpenAi);
        insert_provider(&db, &first).await.unwrap();
        insert_provider(&db, &second).await.unwrap();

        assign_to_pool(&db, "shared", &first).await.unwrap();
        // bump the incumbent to a sparse priority
        crate::pools::queries::upsert_member(
            &db,
            &PoolMember { pool_id: "shared".into(), provider_id: "p1".into(), priority: 10 },
        )
        .await
        .unwrap();

        let prio = assign_to_pool(&db, "shared", &second).await.unwrap();
        assert_eq!(prio, 11, "must go behind the incumbent, not in front of it");
    }

    #[tokio::test]
    async fn assign_to_an_existing_pool_does_not_recreate_it() {
        let db = init_pool(":memory:").await.unwrap();
        let p = provider("p1", WireFormat::OpenAi);
        insert_provider(&db, &p).await.unwrap();
        let created = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        crate::pools::queries::insert_pool(
            &db,
            &crate::core::model::Pool {
                id: "pre".into(),
                wire_format: WireFormat::OpenAi,
                created_at: created,
            },
        )
        .await
        .unwrap();

        assign_to_pool(&db, "pre", &p).await.unwrap();
        // still the original row (a Conflict from a second insert_pool would
        // have surfaced as an Err above)
        assert_eq!(crate::pools::queries::get_pool(&db, "pre").await.unwrap().created_at, created);
    }

    #[test]
    fn passthrough_row_uses_the_name_as_id_and_keeps_kind_passthrough() {
        let p = build_passthrough_row(
            "my-openai",
            WireFormat::OpenAi,
            "https://api.example.com/v1/chat/completions",
            "sk-abc",
            "gpt-4o-mini",
        );
        assert_eq!(p.id, "my-openai");
        assert_eq!(p.name, "my-openai");
        assert_eq!(p.kind, ProviderKind::Passthrough);
        assert_eq!(p.base_url.as_deref(), Some("https://api.example.com/v1/chat/completions"));
        assert_eq!(p.api_key.as_deref(), Some("sk-abc"));
        assert_eq!(p.upstream_model, "gpt-4o-mini");
    }
}
