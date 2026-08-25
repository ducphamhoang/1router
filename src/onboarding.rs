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
pub const CANDIDATE_MODELS: [&str; 5] = [
    "gpt-5.4",
    "gpt-5-codex",
    "gpt-5.1-codex",
    "gpt-5",
    "codex-mini-latest",
];

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
/// testable; the prompts live in the onboarding flows. `model_override` lets the
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
                    strategy: Default::default(),
                    sticky_limit: None,
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
use crate::providers::adapter::commandcode::api_key::commandcode_key_from_disk;
use crate::providers::adapter::commandcode::browser_login::{self, AuthListener, LoginError};
use crate::providers::queries as provider_queries;
use dialoguer::{Confirm, Input, Password, Select};

fn theme() -> dialoguer::theme::ColorfulTheme {
    dialoguer::theme::ColorfulTheme::default()
}

/// Pre-fills wire_format/base_url/upstream_model for a common provider.
/// base_url/api_key/upstream_model stay editable prompts with this as their
/// *default* (press a different key before Enter to override); wire_format
/// is the exception - a preset's base_url IS that wire format, so it's
/// applied directly with no prompt at all (see add_passthrough_provider).
/// Mirrors PROVIDER_TEMPLATES in frontend/src/pages/Providers.tsx - keep the
/// two in sync if either grows.
struct ProviderTemplate {
    label: &'static str,
    wire_format: WireFormat,
    base_url: &'static str,
    upstream_model: &'static str,
    // Only set for templates whose credential is a public, non-secret
    // constant (currently just OpenCode Free's "public" token) - every
    // other template needs a real secret the operator must type, so this
    // stays None for them and the API key prompt has no default.
    api_key: Option<&'static str>,
}

const PROVIDER_TEMPLATES: [ProviderTemplate; 8] = [
    ProviderTemplate {
        label: "OpenAI",
        wire_format: WireFormat::OpenAi,
        base_url: "https://api.openai.com/v1/chat/completions",
        upstream_model: "gpt-5.4",
        api_key: None,
    },
    ProviderTemplate {
        label: "Anthropic",
        wire_format: WireFormat::Anthropic,
        base_url: "https://api.anthropic.com/v1/messages",
        upstream_model: "claude-sonnet-5",
        api_key: None,
    },
    ProviderTemplate {
        label: "DeepSeek (OpenAI-compatible)",
        wire_format: WireFormat::OpenAi,
        base_url: "https://api.deepseek.com/v1/chat/completions",
        upstream_model: "deepseek-flash",
        api_key: None,
    },
    ProviderTemplate {
        label: "DeepSeek (Anthropic-compatible)",
        wire_format: WireFormat::Anthropic,
        base_url: "https://api.deepseek.com/anthropic/v1/messages",
        upstream_model: "deepseek-flash",
        api_key: None,
    },
    ProviderTemplate {
        label: "OpenCode (OpenAI-compatible)",
        wire_format: WireFormat::OpenAi,
        base_url: "https://opencode.ai/zen/go/v1/chat/completions",
        upstream_model: "kimi-k2.7-code",
        api_key: None,
    },
    ProviderTemplate {
        label: "OpenCode (Anthropic-compatible)",
        wire_format: WireFormat::Anthropic,
        base_url: "https://opencode.ai/zen/go/v1/messages",
        upstream_model: "qwen3.7-max",
        api_key: None,
    },
    ProviderTemplate {
        label: "OpenCode Free",
        wire_format: WireFormat::OpenAi,
        base_url: "https://opencode.ai/zen/v1/chat/completions",
        upstream_model: "deepseek-v4-flash-free",
        // Verified live: `Authorization: Bearer public` alone (no extra
        // header) gets a real 200 from this endpoint - see the design
        // spec's "OpenCode Free" section for the curl transcript.
        api_key: Some("public"),
    },
    ProviderTemplate {
        label: "Gemini (OpenAI-compatible)",
        wire_format: WireFormat::OpenAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
        upstream_model: "gemini-2.5-flash",
        api_key: None,
    },
];

/// Turn a template label into a plain lowercase-hyphen id, e.g.
/// "DeepSeek (OpenAI-compatible)" -> "deepseek-openai-compatible".
fn slugify(label: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen && !out.is_empty() {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// A default provider name/id that doesn't collide with one already saved -
/// so picking the same template twice (e.g. two OpenAI keys) doesn't force
/// the operator to type a name, it just suggests "openai-2", "openai-3", ...
async fn unique_default_name(db: &sqlx::SqlitePool, base: &str) -> anyhow::Result<String> {
    if provider_queries::get_provider(db, base).await.is_err() {
        return Ok(base.to_string());
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if provider_queries::get_provider(db, &candidate)
            .await
            .is_err()
        {
            return Ok(candidate);
        }
        n += 1;
    }
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
pub async fn add_passthrough_provider(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
) -> anyhow::Result<Provider> {
    // dialoguer blocks the calling thread. That is fine here and NOT worth
    // wrapping in spawn_blocking: the wizard runs either before the axum
    // listener exists (first boot) or in a process that never starts one
    // (`1router setup`), so there is no concurrent work for it to starve.
    // Template before name: picking a template first lets the name prompt
    // below suggest a sensible default ("openai", "openai-2", ...) instead
    // of asking the operator to invent one. "Custom" is last, not the
    // default choice - most first-time setups pick a real template.
    let mut preset_items: Vec<&str> = PROVIDER_TEMPLATES.iter().map(|p| p.label).collect();
    preset_items.push("Custom");
    let preset_choice = Select::with_theme(&theme())
        .with_prompt("Template (pre-fills the fields below; all stay editable)")
        .items(&preset_items)
        .default(0)
        .interact()?;
    let preset =
        (preset_choice < PROVIDER_TEMPLATES.len()).then(|| &PROVIDER_TEMPLATES[preset_choice]);

    let name_theme = theme();
    let mut name_prompt = Input::<String>::with_theme(&name_theme)
        .with_prompt("Provider name (also used as its id)")
        .validate_with(|s: &String| -> Result<(), &str> {
            if s.trim().is_empty() {
                Err("name cannot be empty")
            } else {
                Ok(())
            }
        });
    // No sensible default for "Custom" - the operator is naming a provider
    // that isn't one of the presets above, so nothing to suggest.
    if let Some(p) = preset {
        name_prompt = name_prompt.default(unique_default_name(db, &slugify(p.label)).await?);
    }
    let name: String = name_prompt.interact_text()?;
    let name = name.trim().to_string();

    // A preset's base_url IS its wire format (it's the exact upstream path
    // for that format) - no point asking the operator to confirm something
    // the template already pinned. Only "Custom" (no preset) leaves the
    // wire format ambiguous enough to need the prompt.
    let wire_format = if let Some(p) = preset {
        p.wire_format
    } else {
        match Select::with_theme(&theme())
            .with_prompt("Wire format")
            .items(["openai", "anthropic"])
            .default(0)
            .interact()?
        {
            0 => WireFormat::OpenAi,
            _ => WireFormat::Anthropic,
        }
    };

    println!(
        "  note: base_url is POSTed as-is - include the full upstream path, \
         e.g. https://api.openai.com/v1/chat/completions"
    );
    let base_url_theme = theme();
    let mut base_url_prompt =
        Input::<String>::with_theme(&base_url_theme).with_prompt("Upstream base_url (full path)");
    if let Some(p) = preset {
        base_url_prompt = base_url_prompt.default(p.base_url.to_string());
    }
    let base_url: String = base_url_prompt.interact_text()?;

    if let Some(default_key) = preset.and_then(|p| p.api_key) {
        println!("  note: this template uses a public, non-secret key ('{default_key}') - press Enter to accept it, or type your own");
    }
    let typed_api_key: String = Password::with_theme(&theme())
        .with_prompt("API key (input hidden)")
        // dialoguer rejects an empty submission and reprompts unless told
        // otherwise - needed here so pressing Enter (accepting a template's
        // default api_key, e.g. OpenCode Free's "public") actually works.
        .allow_empty_password(true)
        .interact()?;
    let api_key = if typed_api_key.trim().is_empty() {
        preset
            .and_then(|p| p.api_key)
            .unwrap_or_default()
            .to_string()
    } else {
        typed_api_key
    };

    let model_theme = theme();
    let mut model_prompt = Input::<String>::with_theme(&model_theme)
        .with_prompt("Upstream model (the real model name this provider expects)");
    if let Some(p) = preset {
        model_prompt = model_prompt.default(p.upstream_model.to_string());
    }
    let upstream_model: String = model_prompt.interact_text()?;

    let mut p = build_passthrough_row(
        &name,
        wire_format,
        base_url.trim(),
        api_key.trim(),
        upstream_model.trim(),
    );
    confirm_upstream_model(http, &mut p).await?;
    provider_queries::insert_provider(db, &p)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create provider '{}': {e}", p.id))?;
    println!("  created provider '{}'", p.id);
    Ok(p)
}

async fn probe_one(
    model: String,
    base: &Provider,
    creds: &crate::providers::adapter::Credentials,
    http: &reqwest::Client,
    body: &bytes::Bytes,
) -> Result<(u16, String), String> {
    let mut candidate = base.clone();
    candidate.upstream_model = model;
    let adapter = adapter_for(&candidate, http.clone());
    let req = adapter
        .build_request(body, creds)
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

/// Known-free model ids drawn from the templates above whose `api_key` is a
/// public, non-secret default (currently just OpenCode Free's
/// "deepseek-v4-flash-free") - the only templates that actually promise a
/// live model works with no real credentials, so those ids are the safest
/// bet among live catalog candidates.
fn known_free_model_ids() -> Vec<&'static str> {
    PROVIDER_TEMPLATES
        .iter()
        .filter(|t| t.api_key.is_some())
        .map(|t| t.upstream_model)
        .collect()
}

/// Live catalogs (e.g. OpenCode Zen's `/v1/models`) commonly list paid
/// models before free ones, so blindly taking the first few candidates
/// tends to grab exactly the ones the current (possibly public/free)
/// api_key can't use - see the OpenCode Zen smoke test where the first 5
/// catalog entries were all paid Claude models and all 401'd. Reorders so
/// the candidates most likely to work with the api_key already on hand
/// come first: 1) ids matching a known free template's own model id, 2)
/// ids whose name contains "free" (OpenCode Zen's own naming convention for
/// its free lineup, e.g. "x-preview-f-free"), 3) everything else - each
/// group keeping its original relative (catalog) order.
fn free_first(models: Vec<String>) -> Vec<String> {
    let known_free = known_free_model_ids();
    let mut known = Vec::new();
    let mut named_free = Vec::new();
    let mut rest = Vec::new();
    for m in models {
        if known_free.contains(&m.as_str()) {
            known.push(m);
        } else if m.to_ascii_lowercase().contains("free") {
            named_free.push(m);
        } else {
            rest.push(m);
        }
    }
    known.into_iter().chain(named_free).chain(rest).collect()
}

/// Verifies the (template-prefilled or hand-typed) upstream_model actually
/// works before inserting, instead of trusting it blindly. A template
/// default can go stale - the upstream side renames or retires a model -
/// with nothing in 1router noticing until a real proxied request fails
/// later. On failure, falls back to the provider's own live `/models` list
/// (the same one "Fetch models" surfaces in the admin UI) and tries a
/// handful of those; if nothing validates, asks the operator to type a
/// replacement, defaulting to what they had so declining just keeps the old
/// (known-unverified) behavior.
///
/// Mirrors `probe_and_set_model`'s probe-first-success shape, but that one
/// exists because a Codex/ChatGPT OAuth account's accepted model name isn't
/// knowable up front - here there is normally exactly one candidate (the
/// template default or what the operator typed), so failure is the
/// exceptional path, not the expected one.
async fn confirm_upstream_model(http: &reqwest::Client, p: &mut Provider) -> anyhow::Result<()> {
    let creds = crate::providers::adapter::Credentials {
        api_key: p.api_key.clone(),
        ..Default::default()
    };
    let body = probe_body();
    let base = p.clone();
    let typed = p.upstream_model.clone();

    println!("  validating upstream_model \"{typed}\"...");
    let outcome = probe_first_success(&[typed.as_str()], |model| {
        probe_one(model, &base, &creds, http, &body)
    })
    .await;

    if let ProbeOutcome::Found(_) = outcome {
        println!("  -> \"{typed}\" works");
        return Ok(());
    }
    let ProbeOutcome::AllFailed(failures) = outcome else {
        unreachable!("probe_first_success only returns Found or AllFailed")
    };
    let (_, status, text) = &failures[0];
    let snippet: String = text.chars().take(300).collect();
    eprintln!(
        "  \"{typed}\" did not work ({status}: {snippet}); checking this provider's own \
         live model list..."
    );

    let candidates: Vec<String> = match crate::providers::routes::fetch_live_models(http, p).await
    {
        Ok(models) => free_first(models.into_iter().filter(|m| m != &typed).collect())
            .into_iter()
            .take(5)
            .collect(),
        Err(e) => {
            eprintln!("  could not fetch a live model list either ({e})");
            Vec::new()
        }
    };

    if !candidates.is_empty() {
        println!(
            "  trying {} live candidate(s) from this provider's own /models...",
            candidates.len()
        );
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let outcome = probe_first_success(&refs, |model| {
            probe_one(model, &base, &creds, http, &body)
        })
        .await;
        if let ProbeOutcome::Found(model) = outcome {
            println!("  -> using \"{model}\" instead (validated live)");
            p.upstream_model = model;
            return Ok(());
        }
    }

    eprintln!(
        "  no candidate model could be validated for this provider right now - you can save \
         it anyway and fix it later via \"Fetch models\" in the admin UI, or:\n    \
         curl -X PATCH .../admin/providers/{} \\\n      \
         -H 'Authorization: Bearer $ROUTER_SHARED_SECRET' \\\n      \
         -d '{{\"upstream_model\":\"<model>\"}}'",
        p.id
    );
    let replacement: String = Input::<String>::with_theme(&theme())
        .with_prompt("Upstream model (nothing validated - keep this or type another)")
        .default(typed)
        .interact_text()?;
    p.upstream_model = replacement.trim().to_string();
    Ok(())
}

pub async fn store_commandcode_key(
    db: &sqlx::SqlitePool,
    provider_id: &str,
    key: &str,
) -> Result<(), AppError> {
    // A new key may belong to a different account type (Go-plan vs Provider
    // API), so drop the in-process transport choice - pi's transport.ts does
    // the same.
    crate::providers::adapter::commandcode::reset_transport(provider_id);
    provider_queries::upsert_oauth_tokens(
        db,
        provider_id,
        Some(key),
        Some(key),
        None,
        Some(browser_login::far_future_expiry()),
        &serde_json::json!({}),
    )
    .await
}

async fn paste_commandcode_key() -> anyhow::Result<String> {
    Ok(tokio::task::spawn_blocking(|| {
        Password::with_theme(&theme())
            .with_prompt("Paste your Command Code API key")
            .interact()
            .map(|key| browser_login::sanitize_api_key(&key))
            .map_err(anyhow::Error::from)
    })
    .await??)
}

/// Run the interactive browser-login flow (open commandcode.ai, listen for
/// the localhost callback), falling back to paste on timeout/bind failure.
async fn prompt_commandcode_key(state_token: &str) -> anyhow::Result<String> {
    match browser_login::bind_listener().await {
        Ok((listener, port)) => {
            let auth = AuthListener::new(listener, port, state_token.to_string());
            let url = auth.authorize_url();
            let task = tokio::spawn(auth.wait());
            println!("\n=== Command Code login ===\nOpen this URL (it is also printed for headless use):\n\n{url}\n");
            browser_login::open_in_browser(&url);
            match task.await? {
                Ok(callback) => browser_login::validate_state(state_token, callback)
                    .map(|callback| callback.api_key)
                    .map_err(|e| anyhow::anyhow!("Command Code login failed: {e:?}")),
                Err(LoginError::Timeout) => {
                    println!("Automatic transfer failed or timed out.");
                    paste_commandcode_key().await
                }
                Err(LoginError::Denied(reason)) => {
                    anyhow::bail!("Command Code login denied: {reason}")
                }
                Err(error) => anyhow::bail!("Command Code login failed: {error:?}"),
            }
        }
        Err(error) => {
            eprintln!("Could not bind the Command Code callback listener ({error}); falling back to paste.");
            paste_commandcode_key().await
        }
    }
}

pub async fn add_commandcode_provider(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
) -> anyhow::Result<Provider> {
    let name_theme = theme();
    let name: String = Input::<String>::with_theme(&name_theme)
        .with_prompt("Provider name (also used as its id)")
        .default(unique_default_name(db, "command-code").await?)
        .validate_with(|s: &String| -> Result<(), &str> {
            if s.trim().is_empty() {
                Err("name cannot be empty")
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    let name = name.trim().to_string();
    // No wire-format prompt here: this adapter bridges Anthropic<->OpenAI
    // itself (see providers::adapter::codex::claude_bridge, reused by the
    // Command Code adapter), so the provider already serves both client
    // formats regardless of what's stored here. The value only matters as
    // the default wire_format for the pool this wizard step may auto-create
    // below; add the provider to a second pool of the other wire_format from
    // the Pools page (or another `1router setup` pass) if you need both
    // routes callable.
    let wire_format = WireFormat::Anthropic;
    let now = chrono::Utc::now();
    let mut provider = Provider {
        id: name.clone(),
        name,
        wire_format,
        kind: ProviderKind::OauthCommandCode,
        base_url: None,
        api_key: None,
        upstream_model: PENDING_MODEL.to_string(),
        created_at: now,
        updated_at: now,
    };
    provider_queries::insert_provider(db, &provider)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create provider '{}': {e}", provider.id))?;

    let state_token = uuid::Uuid::new_v4().to_string();
    let key = if let Some(existing) = commandcode_key_from_disk() {
        if Confirm::with_theme(&theme())
            .with_prompt("Found a Command Code key in ~/.commandcode/auth.json (or env); use it?")
            .default(true)
            .interact()?
        {
            existing
        } else {
            prompt_commandcode_key(&state_token).await?
        }
    } else {
        prompt_commandcode_key(&state_token).await?
    };
    if key.trim().is_empty() {
        anyhow::bail!("Command Code API key cannot be empty");
    }
    store_commandcode_key(db, &provider.id, &key).await?;

    let models = crate::providers::routes::fetch_commandcode_models(http).await;
    let model = match models {
        Ok(models) => {
            let choice = Select::with_theme(&theme())
                .with_prompt("Command Code model")
                .items(&models)
                .default(0)
                .interact()?;
            models[choice].clone()
        }
        Err(reason) => {
            eprintln!("Model discovery failed ({reason}); enter the upstream model manually.");
            Input::<String>::with_theme(&theme())
                .with_prompt("Command Code upstream model")
                .default(PENDING_MODEL.to_string())
                .interact_text()?
        }
    };
    provider_queries::update_provider(
        db,
        &provider.id,
        &ProviderPatch {
            upstream_model: Some(model.clone()),
            ..Default::default()
        },
    )
    .await?;
    provider.upstream_model = model;
    Ok(provider)
}

use crate::providers::adapter::adapter_for;
use crate::providers::adapter::codex::oauth;
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
                &ProviderPatch {
                    upstream_model: Some(model.clone()),
                    ..Default::default()
                },
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
    let creds = crate::providers::adapter::Credentials::from_provider_and_oauth(
        provider,
        provider_queries::get_oauth_state(db, &provider.id)
            .await
            .ok()
            .flatten(),
    );
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
    let name_theme = theme();
    let name: String = Input::<String>::with_theme(&name_theme)
        .with_prompt("Provider name (also used as its id)")
        .default(unique_default_name(db, "codex").await?)
        .validate_with(|s: &String| -> Result<(), &str> {
            if s.trim().is_empty() {
                Err("name cannot be empty")
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    let name = name.trim().to_string();

    // No wire-format prompt here: the Codex adapter bridges Anthropic<->OpenAI
    // itself (claude_bridge), so the provider already serves both client
    // formats regardless of what's stored here. The value only matters as
    // the default wire_format for the pool this wizard step may auto-create
    // below; add the provider to a second pool of the other wire_format from
    // the Pools page (or another `1router setup` pass) if you need both
    // routes callable.
    let wire_format = WireFormat::Anthropic;

    let now = chrono::Utc::now();
    let mut provider = Provider {
        id: name.clone(),
        name,
        wire_format,
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
use std::net::SocketAddr;

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

    // No prompt: use the published default (see README.md) so a brand-new
    // install gets straight to provider setup, TTY or not. It's public
    // information, so main.rs's startup warning is what pushes an operator
    // to rotate it before exposing the admin UI beyond localhost.
    let plain = config::DEFAULT_ADMIN_PASSWORD;
    println!(
        "Admin UI password: using the default 'password' (username: admin, documented in \
         README.md). Change it anytime via `1router setup --reset-admin-password` or the \
         admin UI Settings page."
    );

    let hash = crate::admin::auth::password::hash_password(plain)?;
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

/// Used by main.rs's boot-time warning. Argon2-verifies the stored hash
/// against the published default rather than comparing plaintext anywhere,
/// so this stays correct even though the hash itself was produced with a
/// random salt (see `resolve_or_prompt_admin_password`/`reset_admin_password`).
pub async fn admin_password_is_default(db: &sqlx::SqlitePool) -> anyhow::Result<bool> {
    let hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
            .fetch_optional(db)
            .await?;
    Ok(hash.is_some_and(|h| {
        crate::admin::auth::password::verify_password(&h, config::DEFAULT_ADMIN_PASSWORD)
    }))
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
            // No prompt: use the published default so a brand-new install
            // gets straight to provider setup. It's public information (see
            // README), so main.rs's startup warning - not secrecy - is what
            // pushes an operator to rotate it before going beyond localhost.
            let secret = config::DEFAULT_SHARED_SECRET.to_string();
            config::persist_secret(sqlite_path, &secret)?;
            let path = config::secret_file_path(sqlite_path);
            println!(
                "Admin secret: no secret file yet - using the default '{secret}' \
                 (documented in README.md) so you can get straight to provider setup. \
                 Written to {path:?} (mode 0600)."
            );
            println!(
                "  Use it as `Authorization: Bearer {secret}` on /v1/* and /admin/*. \
                 Change it anytime via `PATCH /admin/settings/shared-secret`, the admin \
                 UI Settings page, or by setting ROUTER_SHARED_SECRET before first boot."
            );
            Ok(secret)
        }
    }
}

pub(crate) fn format_menu_header(
    listen_addr: &SocketAddr,
    sqlite_path: &str,
    provider_count: usize,
    pool_count: usize,
    require_shared_secret: bool,
) -> String {
    let access = if require_shared_secret {
        "required (API key)"
    } else {
        "open (no API key)"
    };
    format!(
        "  listening on {listen_addr}   db: {sqlite_path}\n  {provider_count} providers · {pool_count} pools · /v1 access: {access}"
    )
}

fn auth_mode_parts(source: config::AuthModeSource) -> (bool, crate::core::state::AuthModeOrigin) {
    match source {
        config::AuthModeSource::Env(value) => (value, crate::core::state::AuthModeOrigin::Env),
        config::AuthModeSource::Db(value) => (value, crate::core::state::AuthModeOrigin::Db),
        config::AuthModeSource::Default(value) => {
            (value, crate::core::state::AuthModeOrigin::Default)
        }
    }
}

async fn resolved_auth_mode(
    db: &sqlx::SqlitePool,
    sqlite_path: &str,
) -> anyhow::Result<(bool, crate::core::state::AuthModeOrigin)> {
    let source = config::resolve_shared_secret(sqlite_path)?;
    let mode = config::resolve_auth_mode(
        &source,
        crate::core::settings::get_bool(db, "require_shared_secret").await?,
    )?;
    Ok(auth_mode_parts(mode))
}

fn confirm_open_access(listen_addr: &SocketAddr) -> anyhow::Result<bool> {
    if config::listen_addr_is_loopback(listen_addr) {
        // interact_opt, not interact: Esc/Ctrl-C must fall through to "not
        // confirmed" (same as an explicit No) rather than propagating an
        // error up through the Settings menu.
        return Ok(Confirm::with_theme(&theme())
            .with_prompt("Enable open access? /v1/* will accept requests without an API key")
            .default(false)
            .interact_opt()?
            .unwrap_or(false));
    }

    let typed: String = Input::with_theme(&theme())
        .with_prompt(format!(
            "This gateway listens on {listen_addr}. Type OPEN to enable open access"
        ))
        .interact_text()?;
    Ok(typed == "OPEN")
}

async fn prompt_access_mode(db: &sqlx::SqlitePool, sqlite_path: &str) -> anyhow::Result<()> {
    let (current, origin) = resolved_auth_mode(db, sqlite_path).await?;
    if matches!(origin, crate::core::state::AuthModeOrigin::Env) {
        println!(
            "Access mode is controlled by ROUTER_REQUIRE_SHARED_SECRET; change or unset it to edit this setting."
        );
        return Ok(());
    }
    let secret = resolve_or_prompt_secret(sqlite_path)?;
    let cfg = config::Config::from_env_with_secret(secret)?;
    // interact_opt, not interact: Esc/Ctrl-C returns to the Settings menu
    // with nothing changed, instead of erroring `1router setup` out.
    let Some(choice) = Select::with_theme(&theme())
        .with_prompt("/v1 access mode")
        .items([
            "API key required — clients send Authorization: Bearer <key>",
            "Open access — /v1/* accepts requests with no API key",
        ])
        .default(if current { 0 } else { 1 })
        .interact_opt()?
    else {
        return Ok(());
    };
    let require = choice == 0;
    if require == current {
        return Ok(());
    }
    if !require && !confirm_open_access(&cfg.listen_addr)? {
        println!("Open access was not enabled.");
        return Ok(());
    }
    crate::core::settings::set_bool(db, "require_shared_secret", require).await?;
    Ok(())
}

fn mask_secret_for_cli(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if tail.is_empty() {
        "***".to_string()
    } else {
        format!("***{tail}")
    }
}

fn print_connection_details(require_shared_secret: bool) {
    println!("\nConnection details:\n");
    // `model` can be either a pool id (from the Pools menu) or a direct
    // `<provider-id>/<model>` addressing the provider's own upstream model -
    // both work, the provider's models are listed by GET /v1/models.
    let model = "<pool-id> or <provider-id>/<model>";
    if require_shared_secret {
        println!(
            "  curl http://<host>:<port>/v1/chat/completions \\\n    -H 'Authorization: Bearer <your-admin-secret>' \\\n    -H 'Content-Type: application/json' \\\n    -d '{{\"model\":\"{model}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'\n"
        );
    } else {
        println!(
            "  curl http://<host>:<port>/v1/chat/completions \\\n    # no API key needed — open access is on \\\n    -H 'Content-Type: application/json' \\\n    -d '{{\"model\":\"{model}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'\n"
        );
    }
}

/// The first-boot wizard remains linear; manual `1router setup` uses a menu.
pub async fn run_first_boot_wizard(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    sqlite_path: &str,
) -> anyhow::Result<String> {
    println!("\n=== 1router setup ===\n");
    let secret = resolve_or_prompt_secret(sqlite_path)?;
    prompt_access_mode(db, sqlite_path).await?;

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
            .items([
                "passthrough (OpenAI/Anthropic-compatible API key)",
                "Codex OAuth (ChatGPT account)",
                "Command Code (commandcode.ai browser login)",
            ])
            .default(0)
            .interact()?;

        let provider = match kind {
            0 => add_passthrough_provider(db, http).await?,
            1 => add_codex_provider(db, http).await?,
            _ => add_commandcode_provider(db, http).await?,
        };

        // No pool step anymore: providers are directly callable via
        // `<provider_id>/<model>` (see `pools::select::select_direct_provider`),
        // so clients can use any upstream model without a throwaway pool.
        println!(
            "  added '{}' — call it as model '<{}/<model>' (or add it to a pool from the Pools menu)",
            provider.name, provider.id
        );

        ask = Confirm::with_theme(&theme())
            .with_prompt("Add another provider?")
            .default(false)
            .interact()?;
    }

    println!("\nSetup complete. Example request:\n");
    let (require, _) = resolved_auth_mode(db, sqlite_path).await?;
    print_connection_details(require);
    Ok(secret)
}

async fn run_provider_menu(db: &sqlx::SqlitePool, http: &reqwest::Client) -> anyhow::Result<()> {
    let mut add = Confirm::with_theme(&theme())
        .with_prompt("Add a provider now?")
        .default(true)
        .interact_opt()?
        .unwrap_or(false);
    while add {
        let kind = Select::with_theme(&theme())
            .with_prompt("Provider kind")
            .items([
                "passthrough (OpenAI/Anthropic-compatible API key)",
                "Codex OAuth (ChatGPT account)",
                "Command Code (commandcode.ai browser login)",
            ])
            .default(0)
            .interact_opt()?;
        let Some(kind) = kind else {
            return Ok(());
        };
        let provider = match kind {
            0 => add_passthrough_provider(db, http).await?,
            1 => add_codex_provider(db, http).await?,
            _ => add_commandcode_provider(db, http).await?,
        };
        println!(
            "  added '{}' — call it as model '<{}/<model>' (or add it to a pool from the Pools menu)",
            provider.name, provider.id
        );
        add = Confirm::with_theme(&theme())
            .with_prompt("Add another provider?")
            .default(false)
            .interact_opt()?
            .unwrap_or(false);
    }
    Ok(())
}

async fn run_pool_menu(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let providers = provider_queries::list_providers(db).await?;
    if providers.is_empty() {
        println!("No providers yet. Add one from Providers first.");
        return Ok(());
    }
    let provider_idx = Select::with_theme(&theme())
        .with_prompt("Provider to add to a pool")
        .items(providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>())
        .interact_opt()?;
    let Some(provider_idx) = provider_idx else {
        return Ok(());
    };
    let pool_id: String = Input::with_theme(&theme())
        .with_prompt("Pool id (the model name clients request)")
        .interact_text()?;
    let priority = assign_to_pool(db, pool_id.trim(), &providers[provider_idx], None).await?;
    println!(
        "  added '{}' to pool '{}' at priority {priority}",
        providers[provider_idx].id,
        pool_id.trim()
    );
    Ok(())
}

async fn run_settings_menu(db: &sqlx::SqlitePool, sqlite_path: &str) -> anyhow::Result<()> {
    loop {
        let (require, _) = resolved_auth_mode(db, sqlite_path).await?;
        let current = if require {
            "currently: API key required"
        } else {
            "currently: open (no API key required)"
        };
        let choice = Select::with_theme(&theme())
            .with_prompt("Settings (Esc to go back)")
            .items([
                format!("/v1 access mode          — {current}"),
                "API key (shared secret)  — show or change".to_string(),
                "Admin UI password        — change".to_string(),
                "Back".to_string(),
            ])
            .interact_opt()?;
        let Some(choice) = choice else {
            return Ok(());
        };
        match choice {
            0 => prompt_access_mode(db, sqlite_path).await?,
            1 => {
                if matches!(
                    config::resolve_shared_secret(sqlite_path)?,
                    config::SecretSource::Env(_)
                ) {
                    println!(
                        "API key is controlled by ROUTER_SHARED_SECRET; change or unset the environment variable instead."
                    );
                    continue;
                }
                let secret = resolve_or_prompt_secret(sqlite_path)?;
                println!("Current API key: {}", mask_secret_for_cli(&secret));
                if Confirm::with_theme(&theme())
                    .with_prompt("Rotate the API key?")
                    .default(false)
                    .interact()?
                {
                    let new_secret = Password::with_theme(&theme())
                        .with_prompt("New API key")
                        .with_confirmation("Confirm", "keys did not match")
                        .interact()?;
                    if new_secret.trim().is_empty() {
                        anyhow::bail!("API key cannot be empty");
                    }
                    config::persist_secret(sqlite_path, new_secret.trim())?;
                    println!("API key rotated.");
                }
            }
            2 => reset_admin_password(db).await?,
            _ => return Ok(()),
        }
    }
}

pub async fn run_menu(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    sqlite_path: &str,
) -> anyhow::Result<()> {
    let secret = resolve_or_prompt_secret(sqlite_path)?;
    let cfg = config::Config::from_env_with_secret(secret)?;
    loop {
        let providers = provider_queries::list_providers(db).await?;
        let pools = pool_queries::list_pools(db).await?;
        let (require, _) = resolved_auth_mode(db, sqlite_path).await?;
        println!(
            "\n=== 1router setup ===\n{}\n",
            format_menu_header(
                &cfg.listen_addr,
                sqlite_path,
                providers.len(),
                pools.len(),
                require
            )
        );
        let choice = Select::with_theme(&theme())
            .with_prompt("What do you want to do?")
            .items([
                "Providers   — add or review upstream providers",
                "Pools       — map the `model` names clients request to providers",
                "Settings    — API key, access mode, admin password",
                "Connection details — base URL, model names, example request",
                "Quit",
            ])
            .interact_opt()?;
        let Some(choice) = choice else {
            return Ok(());
        };
        match choice {
            0 => run_provider_menu(db, http).await?,
            1 => run_pool_menu(db).await?,
            2 => run_settings_menu(db, sqlite_path).await?,
            3 => print_connection_details(require),
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::PoolMember;

    fn member(priority: i64) -> PoolMember {
        PoolMember {
            pool_id: "p".into(),
            provider_id: "x".into(),
            priority,
            model_override: None,
        }
    }

    #[test]
    fn menu_header_shows_listener_counts_and_access_mode() {
        assert_eq!(
            format_menu_header(
                &"0.0.0.0:8080".parse().unwrap(),
                "1router.db",
                2,
                3,
                false,
            ),
            "  listening on 0.0.0.0:8080   db: 1router.db\n  2 providers · 3 pools · /v1 access: open (no API key)",
        );
    }

    #[test]
    fn slugify_lowercases_and_hyphenates_punctuation() {
        assert_eq!(slugify("OpenAI"), "openai");
        assert_eq!(
            slugify("DeepSeek (OpenAI-compatible)"),
            "deepseek-openai-compatible"
        );
        assert_eq!(
            slugify("Gemini (OpenAI-compatible)"),
            "gemini-openai-compatible"
        );
    }

    #[tokio::test]
    async fn unique_default_name_reuses_the_base_when_free() {
        let db = init_pool(":memory:").await.unwrap();
        assert_eq!(unique_default_name(&db, "openai").await.unwrap(), "openai");
    }

    #[tokio::test]
    async fn unique_default_name_appends_a_counter_on_collision() {
        let db = init_pool(":memory:").await.unwrap();
        insert_provider(&db, &provider("openai", WireFormat::OpenAi))
            .await
            .unwrap();
        assert_eq!(
            unique_default_name(&db, "openai").await.unwrap(),
            "openai-2"
        );

        insert_provider(&db, &provider("openai-2", WireFormat::OpenAi))
            .await
            .unwrap();
        assert_eq!(
            unique_default_name(&db, "openai").await.unwrap(),
            "openai-3"
        );
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
    fn provider_templates_include_both_opencode_entries() {
        let labels: Vec<&str> = PROVIDER_TEMPLATES.iter().map(|p| p.label).collect();
        assert!(labels.contains(&"OpenCode (OpenAI-compatible)"));
        assert!(labels.contains(&"OpenCode (Anthropic-compatible)"));

        let openai_tmpl = PROVIDER_TEMPLATES
            .iter()
            .find(|p| p.label == "OpenCode (OpenAI-compatible)")
            .unwrap();
        assert_eq!(openai_tmpl.wire_format, WireFormat::OpenAi);
        assert_eq!(
            openai_tmpl.base_url,
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        assert_eq!(openai_tmpl.upstream_model, "kimi-k2.7-code");

        let anthropic_tmpl = PROVIDER_TEMPLATES
            .iter()
            .find(|p| p.label == "OpenCode (Anthropic-compatible)")
            .unwrap();
        assert_eq!(anthropic_tmpl.wire_format, WireFormat::Anthropic);
        assert_eq!(
            anthropic_tmpl.base_url,
            "https://opencode.ai/zen/go/v1/messages"
        );
        assert_eq!(anthropic_tmpl.upstream_model, "qwen3.7-max");
        assert_eq!(openai_tmpl.api_key, None);
        assert_eq!(anthropic_tmpl.api_key, None);
    }

    #[test]
    fn opencode_free_template_defaults_to_the_public_key() {
        let tmpl = PROVIDER_TEMPLATES
            .iter()
            .find(|p| p.label == "OpenCode Free")
            .unwrap();
        assert_eq!(tmpl.wire_format, WireFormat::OpenAi);
        assert_eq!(tmpl.base_url, "https://opencode.ai/zen/v1/chat/completions");
        assert_eq!(tmpl.upstream_model, "deepseek-v4-flash-free");
        assert_eq!(tmpl.api_key, Some("public"));
    }

    #[test]
    fn gemini_template_uses_the_openai_compat_shim() {
        let tmpl = PROVIDER_TEMPLATES
            .iter()
            .find(|p| p.label == "Gemini (OpenAI-compatible)")
            .unwrap();
        assert_eq!(tmpl.wire_format, WireFormat::OpenAi);
        assert_eq!(
            tmpl.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
        assert_eq!(tmpl.upstream_model, "gemini-2.5-flash");
        assert_eq!(tmpl.api_key, None);
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
                if m == "b" {
                    Ok((200, "{}".into()))
                } else {
                    Ok((400, "nope".into()))
                }
            }
        })
        .await;

        assert!(matches!(out, ProbeOutcome::Found(ref m) if m == "b"));
        assert_eq!(&*tried.lock().unwrap(), &["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn probe_reports_every_failure_when_none_succeed() {
        let out =
            probe_first_success(&["a", "b"], |m| async move { Ok((404, format!("no {m}"))) }).await;

        match out {
            ProbeOutcome::AllFailed(fs) => {
                assert_eq!(fs.len(), 2);
                assert_eq!(fs[0], ("a".into(), 404, "no a".into()));
                assert_eq!(fs[1], ("b".into(), 404, "no b".into()));
            }
            ProbeOutcome::Found(m) => panic!("unexpected success: {m}"),
        }
    }

    #[test]
    fn free_first_orders_known_free_then_named_free_then_the_rest() {
        // Mirrors the real OpenCode Zen catalog shape: paid Claude models
        // first, the known-free template default and a differently-named
        // free model further down.
        let ranked = free_first(vec![
            "claude-fable-5".into(),
            "claude-opus-5".into(),
            "deepseek-v4-flash-free".into(), // known-free (OpenCode Free template)
            "gpt-5.4".into(),
            "x-preview-f-free".into(), // named "*free*" but not a known template id
        ]);
        assert_eq!(
            ranked,
            vec![
                "deepseek-v4-flash-free",
                "x-preview-f-free",
                "claude-fable-5",
                "claude-opus-5",
                "gpt-5.4",
            ]
        );
    }

    #[test]
    fn free_first_is_a_no_op_when_nothing_matches() {
        let ranked = free_first(vec!["gpt-5.4".into(), "claude-sonnet-5".into()]);
        assert_eq!(ranked, vec!["gpt-5.4", "claude-sonnet-5"]);
    }

    // confirm_upstream_model's final branch (nothing validates) blocks on an
    // interactive `Input::interact_text()` prompt, which needs a real TTY -
    // not exercisable here. These two cover the non-interactive branches:
    // typed model works immediately, and typed model fails but the
    // provider's own live catalog turns up a working replacement. See
    // docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md for the
    // manual pass covering the reprompt branch.

    #[tokio::test]
    async fn confirm_upstream_model_accepts_a_typed_model_that_probes_clean() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&upstream)
            .await;

        let mut p = provider("p1", WireFormat::OpenAi);
        p.base_url = Some(format!("{}/v1/chat/completions", upstream.uri()));
        p.upstream_model = "gpt-5.4".into();
        let http = reqwest::Client::new();

        confirm_upstream_model(&http, &mut p).await.unwrap();

        assert_eq!(p.upstream_model, "gpt-5.4");
    }

    #[tokio::test]
    async fn confirm_upstream_model_falls_back_to_a_live_catalog_model_when_typed_one_fails() {
        let upstream = wiremock::MockServer::start().await;
        // The typed model 404s on every chat-completion probe...
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("model not found"))
            .mount(&upstream)
            .await;
        // ...but GET /v1/models (derived from the /v1/chat/completions
        // base_url) lists a real replacement.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "live-model-a"}, {"id": "live-model-b"}]
            })))
            .mount(&upstream)
            .await;
        // The live candidates also 404 on chat-completions except one -
        // override with a targeted mock so the second candidate succeeds.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("live-model-b"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .with_priority(1) // must outrank the blanket 404 POST mock above
            .mount(&upstream)
            .await;

        let mut p = provider("p1", WireFormat::OpenAi);
        p.base_url = Some(format!("{}/v1/chat/completions", upstream.uri()));
        p.upstream_model = "stale-model".into();
        let http = reqwest::Client::new();

        confirm_upstream_model(&http, &mut p).await.unwrap();

        assert_eq!(p.upstream_model, "live-model-b");
    }


    #[tokio::test]
    async fn probe_treats_transport_error_as_a_failed_attempt_and_continues() {
        let out = probe_first_success(&["a", "b"], |m| async move {
            if m == "a" {
                Err("connection reset".into())
            } else {
                Ok((200, "{}".into()))
            }
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
            [
                "gpt-5.4",
                "gpt-5-codex",
                "gpt-5.1-codex",
                "gpt-5",
                "codex-mini-latest"
            ]
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

        let pool = crate::pools::queries::get_pool(&db, "my-pool")
            .await
            .unwrap();
        assert_eq!(pool.wire_format, WireFormat::OpenAi);
        let members = crate::pools::queries::list_members(&db, "my-pool")
            .await
            .unwrap();
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
            crate::pools::queries::get_pool(&db, "anth-pool")
                .await
                .unwrap()
                .wire_format,
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
            &PoolMember {
                pool_id: "shared".into(),
                provider_id: "p1".into(),
                priority: 10,
                model_override: None,
            },
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

        let sol_members = crate::pools::queries::list_members(&db, "codex-sol")
            .await
            .unwrap();
        assert_eq!(sol_members[0].provider_id, "codex");
        assert_eq!(
            sol_members[0].model_override.as_deref(),
            Some("gpt-5.6-sol")
        );

        let luna_members = crate::pools::queries::list_members(&db, "codex-luna")
            .await
            .unwrap();
        assert_eq!(
            luna_members[0].model_override.as_deref(),
            Some("gpt-5.6-luna")
        );
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
                strategy: Default::default(),
                sticky_limit: None,
            },
        )
        .await
        .unwrap();

        assign_to_pool(&db, "pre", &p, None).await.unwrap();
        // still the original row (a Conflict from a second insert_pool would
        // have surfaced as an Err above)
        assert_eq!(
            crate::pools::queries::get_pool(&db, "pre")
                .await
                .unwrap()
                .created_at,
            created
        );
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
        assert_eq!(
            p.base_url.as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
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
        let stored = crate::providers::queries::get_provider(&db, "cx")
            .await
            .unwrap();
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
        let stored = crate::providers::queries::get_provider(&db, "cx")
            .await
            .unwrap();
        assert_eq!(stored.upstream_model, PENDING_MODEL);
    }

    #[tokio::test]
    async fn providers_table_emptiness_predicate() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(providers_table_is_empty(&db).await.unwrap());

        insert_provider(&db, &provider("p1", WireFormat::OpenAi))
            .await
            .unwrap();
        assert!(!providers_table_is_empty(&db).await.unwrap());
    }

    #[tokio::test]
    async fn store_commandcode_key_writes_access_and_refresh_and_a_far_future_expiry() {
        let db = init_pool(":memory:").await.unwrap();
        let mut p = provider("cc", WireFormat::OpenAi);
        p.kind = ProviderKind::OauthCommandCode;
        p.api_key = None;
        p.base_url = None;
        insert_provider(&db, &p).await.unwrap();

        store_commandcode_key(&db, "cc", "cc-key-123")
            .await
            .unwrap();
        let state = crate::providers::queries::get_oauth_state(&db, "cc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.access_token.as_deref(), Some("cc-key-123"));
        assert_eq!(state.refresh_token.as_deref(), Some("cc-key-123"));
        assert!(state.id_token.is_none());
        assert_eq!(state.provider_data, serde_json::json!({}));
        assert!(state.access_expires_at.unwrap() > Utc::now() + chrono::Duration::days(3000));
        assert!(crate::providers::queries::get_provider(&db, "cc")
            .await
            .unwrap()
            .api_key
            .is_none());
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

        let row: (i64, String) = sqlx::query_as("SELECT count(*), username FROM admin_users")
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
        assert!(admin_password_is_default(&db).await.unwrap());
    }

    #[tokio::test]
    async fn admin_password_is_default_is_false_once_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin_bootstrap_rotated.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();
        resolve_or_prompt_admin_password(&db).await.unwrap();
        assert!(admin_password_is_default(&db).await.unwrap());

        let hash = crate::admin::auth::password::hash_password("a-real-password").unwrap();
        sqlx::query("UPDATE admin_users SET password_hash = ? WHERE id = 1")
            .bind(&hash)
            .execute(&db)
            .await
            .unwrap();
        assert!(!admin_password_is_default(&db).await.unwrap());
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

        let row: (i64, String) = sqlx::query_as("SELECT count(*), password_hash FROM admin_users")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, "sentinel");
    }
}
