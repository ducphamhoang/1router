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
/// Deliberately takes `pool_id` (and `model_override`) rather than prompting
/// for them, so the whole DB-touching part of the pool step is unit
/// testable; the prompts live in `run_wizard`. `model_override` lets the
/// same already-authenticated provider be reused across several pools that
/// each call a different upstream model (e.g. one Codex OAuth login serving
/// `codex-sol`/`codex-terra`/`codex-luna` pools).
pub async fn assign_to_pool(
    db: &sqlx::SqlitePool,
    pool_id: &str,
    provider: &Provider,
    model_override: Option<String>,
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
            model_override,
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
        .items(["openai", "anthropic"])
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

use crate::providers::adapter::codex::oauth;
use crate::providers::adapter::{adapter_for, Credentials};
use crate::providers::oauth_routes::complete_oauth_exchange;
use crate::providers::queries::ProviderPatch;

/// One minimal chat-completion body, reused for every probe attempt. The
/// adapter rewrites `model` to the provider's upstream_model, so the value
/// here is irrelevant - but it must be present and a string.
fn probe_body() -> bytes::Bytes {
    bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({
            "model": "probe",
            "messages": [{ "role": "user", "content": "Say OK and nothing else." }],
            "max_tokens": 8
        }))
        .unwrap(),
    )
}

/// Mirrors `proxy::flow::credentials_for` (private there); five field copies
/// is not worth a cross-module extraction.
async fn credentials_for(db: &sqlx::SqlitePool, provider: &Provider) -> Credentials {
    match provider_queries::get_oauth_state(db, &provider.id).await {
        Ok(Some(os)) => Credentials {
            api_key: provider.api_key.clone(),
            access_token: os.access_token,
            refresh_token: os.refresh_token,
            id_token: os.id_token,
            access_expires_at: os.access_expires_at,
            provider_data: os.provider_data,
        },
        _ => Credentials {
            api_key: provider.api_key.clone(),
            ..Default::default()
        },
    }
}

pub(crate) async fn persist_probe_result(
    db: &sqlx::SqlitePool,
    provider: &mut Provider,
    outcome: &ProbeOutcome,
) -> anyhow::Result<()> {
    match outcome {
        ProbeOutcome::Found(model) => {
            provider_queries::update_provider(
                db,
                &provider.id,
                &ProviderPatch { upstream_model: Some(model.clone()), ..Default::default() },
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to set upstream_model: {e}"))?;
            provider.upstream_model = model.clone();
        }
        // Not an error per the spec: leave `pending` in place and tell the
        // user how to fix it once they know the right value.
        ProbeOutcome::AllFailed(_) => {}
    }
    Ok(())
}

/// Probe CANDIDATE_MODELS in-process and persist the winner.
///
/// Spec gaps 3+4: the spec's probe went over HTTP through the gateway's own
/// /v1/chat/completions and PATCHed upstream_model per attempt. At wizard time
/// no listener exists, so we build the adapter request directly and mutate an
/// in-memory clone per attempt, persisting only the winner. Same end state.
pub async fn probe_and_set_model(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    provider: &mut Provider,
) -> anyhow::Result<ProbeOutcome> {
    let creds = credentials_for(db, provider).await;
    let body = probe_body();
    println!(
        "Probing which model this ChatGPT account accepts \
         (this sends {} tiny real requests)...",
        CANDIDATE_MODELS.len()
    );

    let outcome = probe_first_success(&CANDIDATE_MODELS, |model| {
        let mut candidate = provider.clone();
        candidate.upstream_model = model.clone();
        let creds = creds.clone();
        let body = body.clone();
        let http = http.clone();
        async move {
            println!("  trying \"{model}\"...");
            let adapter = adapter_for(&candidate, http.clone());
            let req = adapter
                .build_request(&body, &creds)
                .await
                .map_err(|e| format!("request build failed: {e}"))?;
            let resp = http
                .execute(req)
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            Ok((status, text))
        }
    })
    .await;

    match &outcome {
        ProbeOutcome::Found(m) => println!("  -> using upstream_model \"{m}\""),
        ProbeOutcome::AllFailed(failures) => {
            eprintln!("  no candidate model worked; every attempt:");
            for (model, status, body) in failures {
                let body: String = body.chars().take(400).collect();
                eprintln!("    \"{model}\" -> {status}: {body}");
            }
            eprintln!(
                "  leaving upstream_model = \"{PENDING_MODEL}\". Once you know the right \
                 value, set it with:\n    curl -X PATCH .../admin/providers/{} \\\n      \
                 -H 'Authorization: Bearer $ROUTER_SHARED_SECRET' \\\n      \
                 -d '{{\"upstream_model\":\"<model>\"}}'",
                provider.id
            );
        }
    }

    persist_probe_result(db, provider, &outcome).await?;
    Ok(outcome)
}

/// Prompt for a Codex provider: create the row, run the PKCE browser dance,
/// exchange the code, then probe for a working model.
pub async fn add_codex_provider(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
) -> anyhow::Result<Provider> {
    let name: String = Input::with_theme(&theme())
        .with_prompt("Provider name (also used as its id)")
        .validate_with(|s: &String| -> Result<(), &str> {
            if s.trim().is_empty() { Err("name cannot be empty") } else { Ok(()) }
        })
        .interact_text()?;
    let name = name.trim().to_string();

    let now = chrono::Utc::now();
    let mut provider = Provider {
        id: name.clone(),
        name,
        wire_format: WireFormat::OpenAi,
        kind: ProviderKind::OauthCodex,
        base_url: None,
        api_key: None,
        // Replaced by the probe below; kept if every candidate fails.
        upstream_model: PENDING_MODEL.to_string(),
        created_at: now,
        updated_at: now,
    };
    provider_queries::insert_provider(db, &provider)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create provider '{}': {e}", provider.id))?;

    // PKCE + authorize URL, called directly - no HTTP hop through
    // /admin/providers/:id/oauth/start.
    let pkce = oauth::generate_pkce();
    let state_tok = uuid::Uuid::new_v4().to_string();
    provider_queries::store_pkce(db, &provider.id, &pkce.verifier, &state_tok)
        .await
        .map_err(|e| anyhow::anyhow!("failed to store pkce: {e}"))?;
    let url = oauth::build_authorize_url(&state_tok, &pkce.challenge);

    println!(
        "\n=== Codex OAuth ===\n\
         1. Open this URL in a browser and log in to your ChatGPT account:\n\n{url}\n\n\
         2. The browser will be redirected to http://localhost:1455/auth/callback?... \
         which will NOT load - that's expected.\n\
         3. Copy that redirect URL from the address bar and paste it below \
         (a bare `code=...&state=...` also works).\n"
    );

    // Re-prompt on a bad paste or a failed exchange without restarting the
    // whole wizard (spec: error handling).
    loop {
        let pasted: String = Input::with_theme(&theme())
            .with_prompt("Paste the redirect URL")
            .interact_text()?;

        let (code, state) = match parse_code_and_state(&pasted) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  {e}");
                continue;
            }
        };
        match complete_oauth_exchange(db, http, &provider.id, &code, &state).await {
            Ok(()) => {
                println!("  login stored.");
                break;
            }
            Err(e) => {
                eprintln!("  {e} - paste it again (or Ctrl-C to abort)");
                continue;
            }
        }
    }

    probe_and_set_model(db, http, &mut provider).await?;
    Ok(provider)
}

use crate::core::config;
use std::io::IsTerminal;

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Same signal `seed.rs` uses for its own first-boot guard.
pub async fn providers_table_is_empty(db: &sqlx::SqlitePool) -> anyhow::Result<bool> {
    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM providers")
        .fetch_one(db)
        .await?;
    Ok(count.0 == 0)
}

/// Fully separate from resolve_or_prompt_secret: different table, different
/// credential. Same TTY-vs-headless branch shape as that function.
pub async fn resolve_or_prompt_admin_password(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM admin_users")
        .fetch_one(db)
        .await?;
    if count.0 > 0 {
        return Ok(());
    }

    let plain = if stdin_is_tty() {
        let s: String = Password::with_theme(&theme())
            .with_prompt("Set an admin UI password (username: admin)")
            .with_confirmation("Confirm", "passwords did not match")
            .interact()?;
        if s.trim().is_empty() {
            anyhow::bail!("admin password cannot be empty");
        }
        s
    } else {
        let s = config::generate_secret();
        tracing::info!(
            password = %s,
            "generated a new admin UI password (username: admin) - SAVE THIS NOW, it will not be logged again. Change it later via PATCH /admin/auth/password."
        );
        s
    };

    let hash = crate::admin::auth::password::hash_password(&plain)?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO admin_users (id, username, password_hash, updated_at)
         VALUES (1, 'admin', ?, ?)",
    )
    .bind(&hash)
    .bind(&now)
    .execute(db)
    .await?;

    Ok(())
}

/// Overwrites the admin UI password (setting one if none exists yet) and
/// invalidates every existing session. Only reachable via
/// `1router setup --reset-admin-password`, which - like `1router setup`
/// itself - requires a real TTY on stdin.
///
/// Deliberately unauthenticated by design: anyone who can run the CLI on
/// this host already has filesystem access to the sqlite DB and
/// `.router_secret`, so gating this behind the *old* password wouldn't add
/// real protection - it would just remove the only recovery path for an
/// operator who forgot it, forcing a manual `DELETE FROM admin_users` via
/// sqlite3 instead.
pub async fn reset_admin_password(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let plain: String = Password::with_theme(&theme())
        .with_prompt("New admin UI password (username: admin)")
        .with_confirmation("Confirm", "passwords did not match")
        .interact()?;
    if plain.trim().is_empty() {
        anyhow::bail!("admin password cannot be empty");
    }

    let hash = crate::admin::auth::password::hash_password(&plain)?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO admin_users (id, username, password_hash, updated_at)
         VALUES (1, 'admin', ?, ?)
         ON CONFLICT(id) DO UPDATE SET password_hash = excluded.password_hash, updated_at = excluded.updated_at",
    )
    .bind(&hash)
    .bind(&now)
    .execute(db)
    .await?;

    crate::admin::auth::session::delete_all_sessions(db)
        .await
        .map_err(|e| anyhow::anyhow!("password reset but failed to clear old sessions: {e}"))?;

    println!("Admin UI password reset. All existing sessions have been logged out.");
    Ok(())
}

/// Resolve the admin secret, prompting to generate-or-enter one if none
/// exists yet, and persist it to the sidecar file.
///
/// Persisting is what lets a later `1router setup` skip this step entirely.
pub fn resolve_or_prompt_secret(sqlite_path: &str) -> anyhow::Result<String> {
    match config::resolve_shared_secret(sqlite_path)? {
        config::SecretSource::Env(s) => {
            println!("Admin secret: using ROUTER_SHARED_SECRET from the environment.");
            Ok(s)
        }
        config::SecretSource::SidecarFile(s) => {
            println!(
                "Admin secret: reusing {:?}.",
                config::secret_file_path(sqlite_path)
            );
            Ok(s)
        }
        config::SecretSource::BootstrapNeeded => {
            let choice = Select::with_theme(&theme())
                .with_prompt("No admin secret yet. Generate a random one, or enter your own?")
                .items(["Generate a random secret (recommended)", "Enter my own"])
                .default(0)
                .interact()?;
            let secret = if choice == 0 {
                config::generate_secret()
            } else {
                let s: String = Password::with_theme(&theme())
                    .with_prompt("Admin secret (input hidden)")
                    .with_confirmation("Confirm", "secrets did not match")
                    .interact()?;
                let s = s.trim().to_string();
                if s.is_empty() {
                    anyhow::bail!("admin secret cannot be empty");
                }
                s
            };
            // Written before anything else in the wizard proceeds.
            config::persist_secret(sqlite_path, &secret)?;
            let path = config::secret_file_path(sqlite_path);
            println!("Admin secret written to {path:?} (mode 0600).");
            if choice == 0 {
                println!("  Your admin secret is:\n\n    {secret}\n");
                println!(
                    "  Use it as `Authorization: Bearer <secret>` on /v1/* and /admin/*. \
                     It is stored in {path:?}; it will not be printed again."
                );
            }
            Ok(secret)
        }
    }
}

/// The wizard. Shared by the first-boot trigger and `1router setup`.
pub async fn run_wizard(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    sqlite_path: &str,
) -> anyhow::Result<String> {
    println!("\n=== 1router setup ===\n");
    let secret = resolve_or_prompt_secret(sqlite_path)?;

    // On first boot this is always true; via `1router setup` it may not be,
    // in which case we go straight to asking whether to add another one.
    let mut ask = if providers_table_is_empty(db).await? {
        Confirm::with_theme(&theme())
            .with_prompt("Add a provider now?")
            .default(true)
            .interact()?
    } else {
        Confirm::with_theme(&theme())
            .with_prompt("This gateway already has providers. Add another one?")
            .default(true)
            .interact()?
    };

    if !ask {
        println!(
            "Nothing added. Configure providers later via the admin API \
             (POST /admin/providers, POST /admin/pools, \
             PUT /admin/pools/:id/members) - see README.md."
        );
        return Ok(secret);
    }

    while ask {
        let kind = Select::with_theme(&theme())
            .with_prompt("Provider kind")
            .items(["passthrough (OpenAI/Anthropic-compatible API key)",
                     "Codex OAuth (ChatGPT account)"])
            .default(0)
            .interact()?;

        let provider = match kind {
            0 => add_passthrough_provider(db).await?,
            _ => add_codex_provider(db, http).await?,
        };

        // Pool id: what clients will send as `model`. The provider row above
        // is already committed, so an interrupt (Ctrl-C/EOF) here leaves a
        // provider with no pool membership - print a clear recovery hint
        // before propagating rather than a bare prompt error, since the next
        // boot won't re-trigger the wizard (the providers table is no
        // longer empty).
        let default_pool = provider.id.clone();
        let pool_id: String = Input::with_theme(&theme())
            .with_prompt("Pool id (this is the `model` name clients will request)")
            .default(default_pool)
            .interact_text()
            .map_err(|e| {
                anyhow::anyhow!(
                    "setup interrupted: provider '{}' was created but not added to a pool. \
                     Add it later via PUT /admin/pools/:id/members. ({e})",
                    provider.id
                )
            })?;
        let pool_id = pool_id.trim().to_string();
        let priority = assign_to_pool(db, &pool_id, &provider, None).await?;
        println!(
            "  added '{}' to pool '{pool_id}' at priority {priority}",
            provider.id
        );

        // Let the same already-authenticated provider serve more pools under
        // different upstream models (e.g. one Codex OAuth login backing
        // codex-sol/codex-terra/codex-luna) without re-running the OAuth
        // dance or creating duplicate provider rows.
        let mut add_more_pools = Confirm::with_theme(&theme())
            .with_prompt(format!(
                "Add '{}' to another pool with a different model?",
                provider.id
            ))
            .default(false)
            .interact()?;
        while add_more_pools {
            let extra_pool_id: String = Input::with_theme(&theme())
                .with_prompt("Pool id (this is the `model` name clients will request)")
                .interact_text()?;
            let extra_pool_id = extra_pool_id.trim().to_string();

            let model_override: String = Input::with_theme(&theme())
                .with_prompt(format!(
                    "Model override for this pool (blank = use '{}')",
                    provider.upstream_model
                ))
                .allow_empty(true)
                .interact_text()?;
            let model_override = model_override.trim();
            let model_override = if model_override.is_empty() {
                None
            } else {
                Some(model_override.to_string())
            };

            let priority =
                assign_to_pool(db, &extra_pool_id, &provider, model_override.clone()).await?;
            println!(
                "  added '{}' to pool '{extra_pool_id}' at priority {priority}{}",
                provider.id,
                model_override
                    .map(|m| format!(" (model override: '{m}')"))
                    .unwrap_or_default()
            );

            add_more_pools = Confirm::with_theme(&theme())
                .with_prompt(format!(
                    "Add '{}' to yet another pool with a different model?",
                    provider.id
                ))
                .default(false)
                .interact()?;
        }

        ask = Confirm::with_theme(&theme())
            .with_prompt("Add another provider?")
            .default(false)
            .interact()?;
    }

    println!("\nSetup complete. Example request:\n");
    println!(
        "  curl http://<host>:<port>/v1/chat/completions \\\n    \
         -H 'Authorization: Bearer <your-admin-secret>' \\\n    \
         -H 'Content-Type: application/json' \\\n    \
         -d '{{\"model\":\"<pool-id>\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'\n"
    );
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::PoolMember;

    fn member(priority: i64) -> PoolMember {
        PoolMember { pool_id: "p".into(), provider_id: "x".into(), priority, model_override: None }
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

        let prio = assign_to_pool(&db, "my-pool", &p, None).await.unwrap();
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
        assign_to_pool(&db, "anth-pool", &p, None).await.unwrap();
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

        assign_to_pool(&db, "shared", &first, None).await.unwrap();
        // bump the incumbent to a sparse priority
        crate::pools::queries::upsert_member(
            &db,
            &PoolMember { pool_id: "shared".into(), provider_id: "p1".into(), priority: 10, model_override: None },
        )
        .await
        .unwrap();

        let prio = assign_to_pool(&db, "shared", &second, None).await.unwrap();
        assert_eq!(prio, 11, "must go behind the incumbent, not in front of it");
    }

    #[tokio::test]
    async fn assign_with_override_lets_one_provider_serve_several_model_pools() {
        let db = init_pool(":memory:").await.unwrap();
        let p = provider("codex", WireFormat::OpenAi);
        insert_provider(&db, &p).await.unwrap();

        assign_to_pool(&db, "codex-sol", &p, Some("gpt-5.6-sol".into()))
            .await
            .unwrap();
        assign_to_pool(&db, "codex-luna", &p, Some("gpt-5.6-luna".into()))
            .await
            .unwrap();

        let sol_members = crate::pools::queries::list_members(&db, "codex-sol").await.unwrap();
        assert_eq!(sol_members[0].provider_id, "codex");
        assert_eq!(sol_members[0].model_override.as_deref(), Some("gpt-5.6-sol"));

        let luna_members = crate::pools::queries::list_members(&db, "codex-luna").await.unwrap();
        assert_eq!(luna_members[0].model_override.as_deref(), Some("gpt-5.6-luna"));
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

        assign_to_pool(&db, "pre", &p, None).await.unwrap();
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

    #[tokio::test]
    async fn probe_outcome_found_persists_the_winning_model() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = provider("cx", WireFormat::OpenAi);
        p.kind = ProviderKind::OauthCodex;
        p.base_url = None;
        p.api_key = None;
        p.upstream_model = PENDING_MODEL.into();
        insert_provider(&db, &p).await.unwrap();

        persist_probe_result(&db, &mut p, &ProbeOutcome::Found("gpt-5.4".into()))
            .await
            .unwrap();

        assert_eq!(p.upstream_model, "gpt-5.4");
        let stored = crate::providers::queries::get_provider(&db, "cx").await.unwrap();
        assert_eq!(stored.upstream_model, "gpt-5.4");
    }

    #[tokio::test]
    async fn probe_outcome_all_failed_leaves_the_model_pending() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = provider("cx", WireFormat::OpenAi);
        p.kind = ProviderKind::OauthCodex;
        p.upstream_model = PENDING_MODEL.into();
        insert_provider(&db, &p).await.unwrap();

        persist_probe_result(
            &db,
            &mut p,
            &ProbeOutcome::AllFailed(vec![("gpt-5.4".into(), 400, "nope".into())]),
        )
        .await
        .unwrap();

        assert_eq!(p.upstream_model, PENDING_MODEL);
        let stored = crate::providers::queries::get_provider(&db, "cx").await.unwrap();
        assert_eq!(stored.upstream_model, PENDING_MODEL);
    }

    #[tokio::test]
    async fn providers_table_emptiness_predicate() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(providers_table_is_empty(&db).await.unwrap());

        insert_provider(&db, &provider("p1", WireFormat::OpenAi)).await.unwrap();
        assert!(!providers_table_is_empty(&db).await.unwrap());
    }
}

#[cfg(test)]
mod admin_bootstrap_tests {
    use super::*;
    use crate::core::db::init_pool;

    #[tokio::test]
    async fn bootstrap_seeds_admin_user_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin_bootstrap_empty.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();

        resolve_or_prompt_admin_password(&db).await.unwrap();

        let row: (i64, String) =
            sqlx::query_as("SELECT count(*), username FROM admin_users")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, "admin");

        let password_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(!password_hash.trim().is_empty());
        assert_ne!(password_hash, "admin");
    }

    #[tokio::test]
    async fn bootstrap_is_noop_when_admin_user_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin_bootstrap_noop.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();

        sqlx::query(
            "INSERT INTO admin_users (id, username, password_hash, updated_at)
             VALUES (1, 'admin', 'sentinel', '2026-01-01T00:00:00Z')",
        )
        .execute(&db)
        .await
        .unwrap();

        resolve_or_prompt_admin_password(&db).await.unwrap();

        let row: (i64, String) =
            sqlx::query_as("SELECT count(*), password_hash FROM admin_users")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, "sentinel");
    }
}
