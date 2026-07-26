# 1router interactive onboarding wizard — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `1router` usable from a cold start without hand-crafted curl: an
interactive terminal wizard that resolves/persists the admin shared secret,
creates one provider (passthrough **or** Codex OAuth, including automatic
discovery of a working `upstream_model` for the logged-in ChatGPT account),
and assigns it to a pool.

**Design spec (authoritative):** `docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md`

**Architecture:** One new leaf module, `src/onboarding.rs`, plus two edits to
existing files (`src/core/config.rs` for secret resolution,
`src/main.rs` for the two triggers) and one small extraction in
`src/providers/oauth_routes.rs` so the wizard and the HTTP route share one code
-exchange function. **No new HTTP routes, no new business logic.** The wizard is
a sequencing layer over `providers::queries::*`, `pools::queries::*`,
`providers::adapter::codex::oauth::*` and `providers::adapter::adapter_for`.
Module dependency direction is unchanged: `onboarding` → {`core`, `providers`,
`pools`}, and nothing depends on `onboarding` except `main.rs`.

**Tech Stack:** unchanged, plus one new dependency: `dialoguer` (text input,
masked password input, select, confirm). No `clap` — the single `setup`
subcommand is a `std::env::args().nth(1)` check, per the spec's non-goals.

---

## Global Constraints

- **`cargo fetch` must be run with real network access before any offline
  work begins on this phase.** `dialoguer` is a new dependency and every
  build/test in this repo runs `--offline` against a pre-populated
  `~/.cargo/registry` (see `CLAUDE.md` → "Known Codex sandbox limitations").
  Task P5-1 is the only task that adds a dependency; do it first, on a machine
  with network, and commit the resulting `Cargo.lock`.
- **axum stays pinned at 0.7** — irrelevant here (no new routes) but do not
  bump it opportunistically.
- **Never log or echo the shared secret** except in the single, deliberate
  no-TTY bootstrap `info!` line the spec mandates ("save this now, it will not
  be logged again"). The `api_key` prompt uses `dialoguer::Password` (masked)
  and its value is never printed back.
- **The wizard only adds.** No interactive edit/delete of existing providers,
  pools, or members.
- **Never block on stdin without a TTY.** Every interactive entry point is
  gated on `std::io::IsTerminal::is_terminal(&std::io::stdin())`.
- **All prompt-free logic must be extracted as plain functions** so it is unit
  testable without a terminal or network (`next_priority`,
  `parse_code_and_state`, `probe_first_success`, `assign_to_pool`). Only the
  thin `dialoguer` sequencing functions are untested-by-machine, and they get
  the manual smoke checklist at the end of this plan instead.
- `dialoguer` prompts are **blocking** and run on a tokio runtime thread. Wrap
  each prompt in `tokio::task::spawn_blocking` **or** accept that the wizard
  runs before/around any concurrent work (it does — nothing else is running at
  wizard time). This plan uses direct blocking calls and documents why (see
  P5-5, Step 3 note); do not "fix" it into `spawn_blocking` without reason,
  it only adds noise.

---

## Spec gaps and deviations (flagged, not silent)

The spec is authoritative. Four places where it does not match the code as it
actually exists on `impl/v1`; each is resolved here in the way that preserves
the spec's intent, and each is called out again inline at the task that hits it.

1. **`providers::queries::create_provider` does not exist.** The spec names it
   three times. The real API is
   `providers::queries::insert_provider(db: &SqlitePool, p: &Provider) -> Result<(), AppError>`
   (the `POST /admin/providers` handler builds the `Provider` struct itself,
   see `src/providers/routes.rs::create`). This plan uses `insert_provider` and
   constructs `Provider` the same way `routes::create` does. Similarly the
   pool functions are `pools::queries::insert_pool(db, &Pool)` and
   `pools::queries::upsert_member(db, &PoolMember)` — there is no
   `create_pool`/`add_member`.

2. **`oauth_routes::complete` is a private axum handler**, so "the same
   function `oauth_routes::complete`'s handler calls" cannot be called from the
   wizard as written: the handler inlines the state check, `exchange_code`,
   `decode_account_claims`, `upsert_oauth_tokens` and `clear_pkce`. Task P5-6
   **extracts** that body into
   `pub async fn complete_oauth_exchange(db, http, provider_id, code, state) -> Result<(), AppError>`
   in `src/providers/oauth_routes.rs`, and rewrites the handler to call it
   (plus `reload_snapshot`, which is HTTP-only and stays in the handler). This
   is the only way to honour the spec's "no new business logic — reuse the same
   function" requirement without duplicating five statements.

3. **The model probe cannot go through `/v1/chat/completions`.** The spec's
   probe mirrors `tests/e2e_real_providers.rs::codex_end_to_end_real`, which
   PATCHes `upstream_model` over HTTP and POSTs to the gateway's own
   `/v1/chat/completions`. At wizard time on first boot **the axum listener is
   not up yet** (the wizard runs before `load_snapshot`, and `setup` never
   starts a server at all), so there is no local endpoint to POST to. The
   probe therefore runs **in-process**:
   `adapter_for(&candidate_provider, http).build_request(&body, &creds)` then
   `http.execute(req)`, checking the upstream status directly. Same candidate
   list, same stop-at-first-200 semantics, same reported outcome.
4. Consequence of (3): the spec says "setting `upstream_model` via
   `queries::update_provider` before each attempt". Since the probe builds its
   own adapter from a `Provider` value, this plan mutates an **in-memory
   clone** per attempt and calls `update_provider` **once** with the winner.
   Observable end state is identical (`upstream_model` = first model that
   returned 200, or `"pending"` if none did) with N-1 fewer DB writes and no
   window where the persisted row advertises a model that was never confirmed.

---

## Phase 5 — Interactive onboarding wizard

**Parallelism:** P5-1 (dependency + module stub) must land first — everything
imports from it. Then **P5-2 (secret resolution in `Config`)**, **P5-3 (pure
helpers)** and **P5-4 (pool assignment)** are three-way leaf-parallel: they
touch disjoint code (`core/config.rs`, `onboarding.rs` helpers section,
`onboarding.rs` pool section — the two `onboarding.rs` tasks append to
different regions of one file, so if they run in parallel worktrees expect one
trivial hand-merge, exactly like Phase 1's `providers/mod.rs` collisions). P5-5
(passthrough flow) depends on P5-3+P5-4. P5-6 (Codex flow) depends on P5-3+P5-4
and additionally refactors `oauth_routes.rs`. P5-7 (`run_wizard` top-level
loop) joins P5-5+P5-6. P5-8 (`main.rs` wiring) joins P5-2+P5-7. P5-9 (docs +
manual smoke) is last. Treat P5-7 and P5-8 as the join points.

---

### Task P5-1: Add `dialoguer` dependency + `onboarding` module stub

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock` (regenerated)
- Create: `src/onboarding.rs`
- Modify: `src/lib.rs` (add `pub mod onboarding;`)

**Interfaces:**
- Consumes: nothing.
- Produces: a compiling, empty `router::onboarding` module and the
  `dialoguer` crate in the offline registry for every later task.

- [ ] **Step 1: Write the failing test**

There is nothing to unit test yet; the test is that the crate compiles with
the new dependency and the new module is reachable. Add to `src/onboarding.rs`:

```rust
//! Interactive terminal onboarding wizard.
//!
//! Design: docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md
//!
//! This module contains no business logic of its own: it sequences calls into
//! `providers::queries`, `pools::queries` and
//! `providers::adapter::codex::oauth`, and owns only the prompt UI plus a few
//! pure helpers (which is where all of its unit tests live).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_reachable() {
        // Placeholder: replaced by real helper tests in P5-3.
        assert!(true);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `file not found for module `onboarding`` / unresolved module,
because `src/lib.rs` does not declare it yet.

- [ ] **Step 3: Write minimal implementation**

**This step requires real network access.** Run:

```bash
cargo add dialoguer
cargo fetch
```

`dialoguer` 0.11 is the expected resolution. Confirm `Cargo.toml`'s
`[dependencies]` now contains a `dialoguer = "0.11"` line (default features
are fine — they include `Input`, `Password`, `Select`, `Confirm`). If you must
do this offline, the dependency cannot be added at all; stop and get network
access first (see Global Constraints).

Add to `src/lib.rs`, keeping the list alphabetical:

```rust
pub mod onboarding;
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo build --offline && cargo test --offline --lib onboarding`
Expected: PASS — 1 test (`module_is_reachable`). The build must succeed
**offline**, which proves `cargo fetch` populated the registry; if it fails
with a network error, Step 3's `cargo fetch` did not run with real network.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/onboarding.rs src/lib.rs
git commit -m "chore: add dialoguer dep + onboarding module stub

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-2: Shared-secret resolution — env → sidecar file → bootstrap-needed

**Files:**
- Modify: `src/core/config.rs`

**Interfaces:**
- Consumes: `ROUTER_SHARED_SECRET`, `ROUTER_SQLITE_PATH` (both already read by
  `Config::from_env`); `rand` (already a dependency, used by
  `codex::oauth::generate_pkce`).
- Produces:
  ```rust
  /// Where a resolved admin secret came from, or that none exists yet.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum SecretSource {
      /// ROUTER_SHARED_SECRET was set. Always wins.
      Env(String),
      /// Read from the `.router_secret` sidecar next to the SQLite file.
      SidecarFile(String),
      /// Neither exists: caller must generate-or-prompt, then persist.
      BootstrapNeeded,
  }

  /// `<dir containing sqlite_path>/.router_secret`
  pub fn secret_file_path(sqlite_path: &str) -> std::path::PathBuf;

  /// env -> sidecar -> BootstrapNeeded. A *present but unreadable* sidecar
  /// file is a hard error (never silently regenerated - that would invalidate
  /// a secret already handed to real callers).
  pub fn resolve_shared_secret(sqlite_path: &str) -> anyhow::Result<SecretSource>;

  /// 32 CSPRNG bytes, lowercase hex (64 chars).
  pub fn generate_secret() -> String;

  /// Write the sidecar file with mode 0600, creating parent dirs as needed.
  pub fn persist_secret(sqlite_path: &str, secret: &str) -> anyhow::Result<()>;

  impl Config {
      /// Build a Config from env with an already-resolved secret.
      pub fn from_env_with_secret(secret: String) -> anyhow::Result<Config>;
      /// Unchanged signature. Resolves the secret itself and errors on
      /// BootstrapNeeded, so existing callers/tests keep working.
      pub fn from_env() -> anyhow::Result<Config>;
  }
  ```
  The split exists because the spec requires the bootstrap-needed case to be a
  **distinct return variant** so `main.rs` can act on it, while `Config` itself
  stays immutable and complete once constructed.

- [ ] **Step 1: Write the failing test**

Replace the `mod tests` block in `src/core/config.rs` with (keeping the two
existing tests, one of them fixed — see the note below):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env is process-global; cargo runs #[test] fns on multiple threads by
    // default, so tests that set/remove env vars must serialize on this lock or
    // they race each other's ROUTER_* variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_reads_required_and_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ROUTER_LISTEN_ADDR", "127.0.0.1:9999");
        std::env::set_var("ROUTER_SQLITE_PATH", "/tmp/x.db");
        std::env::set_var("ROUTER_SHARED_SECRET", "s3cret");
        std::env::remove_var("ROUTER_SEED_PATH");

        let c = Config::from_env().unwrap();
        assert_eq!(c.listen_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(c.sqlite_path, "/tmp/x.db");
        assert_eq!(c.shared_secret, "s3cret");
        assert!(c.seed_path.is_none());
        assert_eq!(c.connect_timeout, std::time::Duration::from_secs(10));
        assert_eq!(c.max_body_bytes, 10 * 1024 * 1024);

        std::env::remove_var("ROUTER_SHARED_SECRET");
    }

    #[test]
    fn from_env_errors_without_secret_or_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Must be a fresh tempdir, NOT /tmp/x.db: the sidecar for /tmp/x.db is
        // /tmp/.router_secret, which a previous real run on this machine may
        // have created - it would make this test pass/fail depending on
        // unrelated local state.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        std::env::set_var("ROUTER_SQLITE_PATH", db.to_str().unwrap());
        std::env::remove_var("ROUTER_SHARED_SECRET");
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn resolve_prefers_env_over_sidecar_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        let db = db.to_str().unwrap();
        persist_secret(db, "from-file").unwrap();
        std::env::set_var("ROUTER_SHARED_SECRET", "from-env");

        assert_eq!(
            resolve_shared_secret(db).unwrap(),
            SecretSource::Env("from-env".into())
        );
        std::env::remove_var("ROUTER_SHARED_SECRET");
    }

    #[test]
    fn resolve_reads_sidecar_file_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        let db = db.to_str().unwrap();
        // trailing newline must be trimmed (people edit this file by hand)
        std::fs::write(secret_file_path(db), "  from-file\n").unwrap();
        std::env::remove_var("ROUTER_SHARED_SECRET");

        assert_eq!(
            resolve_shared_secret(db).unwrap(),
            SecretSource::SidecarFile("from-file".into())
        );
    }

    #[test]
    fn resolve_signals_bootstrap_needed_when_neither_exists() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        std::env::remove_var("ROUTER_SHARED_SECRET");

        assert_eq!(
            resolve_shared_secret(db.to_str().unwrap()).unwrap(),
            SecretSource::BootstrapNeeded
        );
    }

    #[test]
    fn secret_file_sits_next_to_the_db_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sub").join("r.db");
        let db = db.to_str().unwrap();
        assert_eq!(secret_file_path(db), std::path::Path::new(db).parent().unwrap().join(".router_secret"));

        persist_secret(db, "abc").unwrap();
        assert_eq!(std::fs::read_to_string(secret_file_path(db)).unwrap(), "abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(secret_file_path(db)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "sidecar must be owner-read/write only");
        }
    }

    #[test]
    fn secret_file_path_handles_bare_relative_filename() {
        // The default sqlite_path is "1router.db" - no parent component at all.
        assert_eq!(
            secret_file_path("1router.db"),
            std::path::Path::new(".router_secret")
        );
    }

    #[test]
    fn generated_secret_is_64_hex_chars_and_unique() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
```

Note: `tempfile` is already a `[dev-dependencies]` entry, so no new dep.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib core::config`
Expected: FAIL — `cannot find function `persist_secret``,
`cannot find function `resolve_shared_secret``, `cannot find type
`SecretSource`` etc. (compile error, not an assertion failure).

- [ ] **Step 3: Write minimal implementation**

In `src/core/config.rs`, add above `impl Config`:

```rust
pub const SECRET_FILE_NAME: &str = ".router_secret";

/// Where a resolved admin secret came from, or that none exists yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    Env(String),
    SidecarFile(String),
    BootstrapNeeded,
}

impl SecretSource {
    pub fn into_secret(self) -> Option<String> {
        match self {
            SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s),
            SecretSource::BootstrapNeeded => None,
        }
    }
}

pub fn secret_file_path(sqlite_path: &str) -> PathBuf {
    // `Path::parent()` of a bare filename is Some(""), which joins correctly
    // to a plain relative ".router_secret"; None only happens for a path that
    // terminates in a root/prefix, where CWD is the only sane fallback.
    match std::path::Path::new(sqlite_path).parent() {
        Some(dir) => dir.join(SECRET_FILE_NAME),
        None => PathBuf::from(SECRET_FILE_NAME),
    }
}

/// env -> sidecar file -> BootstrapNeeded.
///
/// A sidecar file that exists but cannot be read is a fail-fast error: we must
/// never silently generate a replacement, because whatever secret it held has
/// already been handed out to real callers.
pub fn resolve_shared_secret(sqlite_path: &str) -> anyhow::Result<SecretSource> {
    if let Ok(s) = std::env::var("ROUTER_SHARED_SECRET") {
        if !s.is_empty() {
            return Ok(SecretSource::Env(s));
        }
    }
    let path = secret_file_path(sqlite_path);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!("secret file {path:?} is empty; delete it to re-bootstrap, or set ROUTER_SHARED_SECRET");
            }
            Ok(SecretSource::SidecarFile(trimmed.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SecretSource::BootstrapNeeded),
        Err(e) => Err(anyhow::anyhow!("failed to read secret file {path:?}: {e}")),
    }
}

/// 32 CSPRNG bytes, lowercase hex.
pub fn generate_secret() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write the sidecar file with owner-only permissions.
pub fn persist_secret(sqlite_path: &str, secret: &str) -> anyhow::Result<()> {
    let path = secret_file_path(sqlite_path);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(&path, secret)
        .map_err(|e| anyhow::anyhow!("failed to write secret file {path:?}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("failed to chmod secret file {path:?}: {e}"))?;
    }
    Ok(())
}
```

Then restructure `impl Config` so the secret is injected rather than read
inline:

```rust
impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let sqlite_path = sqlite_path_from_env();
        let secret = resolve_shared_secret(&sqlite_path)?
            .into_secret()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no admin secret: set ROUTER_SHARED_SECRET, or run `1router setup` \
                     to create {:?}",
                    secret_file_path(&sqlite_path)
                )
            })?;
        Config::from_env_with_secret(secret)
    }

    pub fn from_env_with_secret(shared_secret: String) -> anyhow::Result<Config> {
        let listen_addr = std::env::var("ROUTER_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()?;
        let seed_path = std::env::var("ROUTER_SEED_PATH").ok().map(PathBuf::from);
        let max_body_bytes = std::env::var("ROUTER_MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024);

        Ok(Config {
            listen_addr,
            sqlite_path: sqlite_path_from_env(),
            shared_secret,
            seed_path,
            connect_timeout: env_secs("ROUTER_CONNECT_TIMEOUT", 10),
            ttfb_timeout: env_secs("ROUTER_TTFB_TIMEOUT", 60),
            idle_timeout: env_secs("ROUTER_IDLE_TIMEOUT", 120),
            max_body_bytes,
            drain_timeout: env_secs("ROUTER_DRAIN_TIMEOUT", 30),
        })
    }
}

/// Read once, in one place: `main.rs` needs the DB path *before* it can
/// resolve the secret (the sidecar lives next to the DB file).
pub fn sqlite_path_from_env() -> String {
    std::env::var("ROUTER_SQLITE_PATH").unwrap_or_else(|_| "1router.db".to_string())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib core::config`
Expected: PASS — 8 tests.
Then run: `cargo test --offline`
Expected: PASS — the whole suite. `Config::from_env`'s signature is unchanged,
so `tests/common` and `src/seed.rs`'s tests (which construct `Config` directly)
are unaffected. If `tests/startup.rs` or `tests/common` regress, they were
depending on the old inline env read — fix them to use
`from_env_with_secret`, not by reverting this task.

- [ ] **Step 5: Commit**

```bash
git add src/core/config.rs
git commit -m "feat(config): resolve admin secret via env then .router_secret sidecar

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-3: Pure wizard helpers — priority defaulting, paste parsing, model probe

**Files:**
- Modify: `src/onboarding.rs`

**Interfaces:**
- Consumes: `core::model::PoolMember` (P0-3).
- Produces:
  ```rust
  /// Candidate Codex models, in probe order. Kept in sync (by hand) with
  /// tests/e2e_real_providers.rs::codex_end_to_end_real.
  pub const CANDIDATE_MODELS: [&str; 5] =
      ["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5", "codex-mini-latest"];

  pub const PENDING_MODEL: &str = "pending";

  /// 1 for an empty pool, else max(existing priority) + 1 - so a new member
  /// never silently jumps the queue in front of an existing provider.
  pub fn next_priority(existing: &[PoolMember]) -> i64;

  /// Accept either a full pasted redirect URL or a bare `code=..&state=..`
  /// fragment. Same logic already proven in the e2e test.
  pub fn parse_code_and_state(input: &str) -> anyhow::Result<(String, String)>;

  pub enum ProbeOutcome {
      /// First model that returned 200.
      Found(String),
      /// Every candidate failed: (model, status, body) per attempt.
      AllFailed(Vec<(String, u16, String)>),
  }

  /// Try each model in order via `attempt`, stop at the first 200.
  /// `attempt` returns (status, body) or a transport error string.
  pub async fn probe_first_success<F, Fut>(models: &[&str], mut attempt: F) -> ProbeOutcome
  where
      F: FnMut(String) -> Fut,
      Fut: std::future::Future<Output = Result<(u16, String), String>>;
  ```
  All four are terminal- and network-free, which is the whole point: the probe
  loop's control flow is tested with a fake `attempt` closure.

- [ ] **Step 1: Write the failing test**

Replace `src/onboarding.rs`'s placeholder `mod tests` with:

```rust
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
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `cannot find function `next_priority``, `cannot find function
`parse_code_and_state``, `cannot find function `probe_first_success``,
`cannot find type `ProbeOutcome``, `cannot find value `CANDIDATE_MODELS``.

- [ ] **Step 3: Write minimal implementation**

Add to `src/onboarding.rs`, above the test module:

```rust
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
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding`
Expected: PASS — 10 tests.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(onboarding): pure helpers - priority, paste parsing, model probe loop

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-4: Pool assignment (create-if-missing + append member)

**Files:**
- Modify: `src/onboarding.rs`

**Interfaces:**
- Consumes: `pools::queries::{get_pool, insert_pool, list_members, upsert_member}`
  (P1-5/P1-6); `core::model::{Pool, PoolMember, Provider, WireFormat}`;
  `next_priority` (P5-3). **Note (spec gap 1):** the functions are
  `insert_pool`/`upsert_member`, not `create_pool`/`add_member`.
- Produces:
  ```rust
  /// Add `provider` to pool `pool_id`, creating the pool (with the provider's
  /// wire_format) if it does not exist. Returns the priority assigned.
  /// No prompting: takes pool_id as an argument so it is unit testable.
  pub async fn assign_to_pool(
      db: &sqlx::SqlitePool,
      pool_id: &str,
      provider: &Provider,
  ) -> anyhow::Result<i64>;
  ```

- [ ] **Step 1: Write the failing test**

Add to `src/onboarding.rs`'s `mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `cannot find function `assign_to_pool``.

- [ ] **Step 3: Write minimal implementation**

Add to `src/onboarding.rs`:

```rust
use crate::core::model::{Pool, Provider};
use crate::core::error::AppError;
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
```

Watch the import list — `PoolMember` is already imported by P5-3; don't
duplicate it.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding`
Expected: PASS — 14 tests.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(onboarding): pool assignment with max+1 priority defaulting

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-5: Passthrough provider prompt flow

**Files:**
- Modify: `src/onboarding.rs`

**Interfaces:**
- Consumes: `dialoguer::{Input, Password, Select}`;
  `providers::queries::insert_provider` (**spec gap 1** — not
  `create_provider`); `core::model::{Provider, ProviderKind, WireFormat}`.
- Produces:
  ```rust
  /// Prompt for name / wire_format / base_url / api_key / upstream_model and
  /// insert the provider. Returns the inserted row.
  pub async fn add_passthrough_provider(db: &sqlx::SqlitePool) -> anyhow::Result<Provider>;
  ```
  `name` is used as **both** `Provider.id` and `Provider.name`, per the spec.

- [ ] **Step 1: Write the failing test**

The `dialoguer` calls are not practically unit testable (they read a real
terminal) — per the spec's testing section this function is covered by the
manual smoke checklist in P5-9, and by the fact that everything it calls
(`insert_provider`) is already covered by `src/providers/queries.rs`'s tests
and `tests/admin_*`. The machine-checkable assertion for this task is that it
compiles and that the crate's existing tests still pass.

Optional but recommended guard test to add to `mod tests` — it pins the
id/name doubling and the `Provider` shape the flow builds, without touching a
terminal (extract the row construction so it can be asserted):

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `cannot find function `build_passthrough_row``.

- [ ] **Step 3: Write minimal implementation**

Add to `src/onboarding.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding && cargo build --offline`
Expected: PASS — 15 tests, clean build. (`Confirm` is imported here but only
used in P5-7; if the build warns about an unused import, add it in P5-7
instead.)

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(onboarding): passthrough provider prompt flow

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-6: Codex OAuth flow + in-process model probe

**Files:**
- Modify: `src/providers/oauth_routes.rs` (extract shared exchange fn)
- Modify: `src/onboarding.rs`

**Interfaces:**
- Consumes: `oauth::{generate_pkce, build_authorize_url}`;
  `providers::queries::{store_pkce, get_oauth_state, update_provider, ProviderPatch}`;
  `providers::adapter::{adapter_for, Credentials, ProviderAdapter}`;
  `parse_code_and_state` + `probe_first_success` + `CANDIDATE_MODELS` +
  `PENDING_MODEL` (P5-3).
- Produces:
  ```rust
  // in src/providers/oauth_routes.rs - EXTRACTED from the existing private
  // `complete` handler, which is rewritten to call it (spec gap 2).
  pub async fn complete_oauth_exchange(
      db: &sqlx::SqlitePool,
      http: &reqwest::Client,
      provider_id: &str,
      code: &str,
      state: &str,
  ) -> Result<(), AppError>;

  // in src/onboarding.rs
  pub async fn add_codex_provider(
      db: &sqlx::SqlitePool,
      http: &reqwest::Client,
  ) -> anyhow::Result<Provider>;

  /// Probe CANDIDATE_MODELS in-process against the stored OAuth token and
  /// persist the winner. Returns the ProbeOutcome (never an Err for a
  /// no-model-worked result - that is a normal outcome per the spec).
  pub async fn probe_and_set_model(
      db: &sqlx::SqlitePool,
      http: &reqwest::Client,
      provider: &mut Provider,
  ) -> anyhow::Result<ProbeOutcome>;
  ```

- [ ] **Step 1: Write the failing test**

First, a regression test that the extraction preserved behaviour — the
existing `tests/codex_oauth.rs` integration test already exercises
`POST /admin/providers/:id/oauth/complete` end to end against wiremock, so it
**is** the test for the refactor; do not write a new one. Add only the
probe-persistence unit test to `src/onboarding.rs`'s `mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `cannot find function `persist_probe_result``.

- [ ] **Step 3: Write minimal implementation**

**3a — extract the shared exchange (spec gap 2).** In
`src/providers/oauth_routes.rs`, move the body of `complete` into a public
function and leave the handler as a thin wrapper. The only thing that stays
in the handler is `reload_snapshot`, which needs `AppState` and is
HTTP-request-scoped (the wizard reloads nothing — on first boot `load_snapshot`
runs after the wizard, and `1router setup` exits without serving):

```rust
/// Validate `state`, exchange `code`, persist tokens, clear the PKCE row.
///
/// Extracted from the `complete` handler so the onboarding wizard can run the
/// exact same exchange in-process (no HTTP hop) instead of duplicating it.
pub async fn complete_oauth_exchange(
    db: &sqlx::SqlitePool,
    http: &reqwest::Client,
    provider_id: &str,
    code: &str,
    state: &str,
) -> Result<(), AppError> {
    let os = queries::get_oauth_state(db, provider_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("no oauth flow in progress; call start first".into()))?;
    let verifier = os
        .pkce_verifier
        .ok_or_else(|| AppError::BadRequest("missing pkce verifier".into()))?;
    let expected_state = os
        .oauth_state
        .ok_or_else(|| AppError::BadRequest("missing oauth state; call start first".into()))?;
    if state != expected_state {
        return Err(AppError::BadRequest("state mismatch".into()));
    }

    let tokens = oauth::exchange_code(http, code, &verifier)
        .await
        .map_err(|e| AppError::BadRequest(format!("code exchange failed: {e}")))?;

    let mut provider_data = serde_json::json!({});
    if let Some(idt) = &tokens.id_token {
        let claims = oauth::decode_account_claims(idt);
        if let Some(acct) = claims.chatgpt_account_id {
            provider_data["chatgpt_account_id"] = json!(acct);
        }
        if let Some(ws) = claims.workspace_id {
            provider_data["workspace_id"] = json!(ws);
        }
    }
    let expires_at = tokens
        .expires_in
        .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s));

    queries::upsert_oauth_tokens(
        db,
        provider_id,
        Some(&tokens.access_token),
        tokens.refresh_token.as_deref(),
        tokens.id_token.as_deref(),
        expires_at,
        &provider_data,
    )
    .await?;
    queries::clear_pkce(db, provider_id).await?;
    Ok(())
}

async fn complete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CompleteBody>,
) -> Result<Json<Value>, AppError> {
    complete_oauth_exchange(&s.db, &s.http, &id, &b.code, &b.state).await?;
    reload_snapshot(&s).await?;
    Ok(Json(json!({ "status": "ok" })))
}
```

**3b — the wizard's Codex flow.** Add to `src/onboarding.rs`:

```rust
use crate::providers::adapter::{adapter_for, Credentials};
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
```

`uuid` is already a dependency (used by `oauth_routes::start`). `bytes` too.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding`
Expected: PASS — 17 tests.
Run: `cargo test --offline --test codex_oauth`
Expected: PASS — unchanged; this is the regression check on the
`complete_oauth_exchange` extraction.
Run: `cargo test --offline`
Expected: PASS — whole suite.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs src/providers/oauth_routes.rs
git commit -m "feat(onboarding): Codex OAuth flow with in-process model probe

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-7: `run_wizard` — secret bootstrap, provider loop, pool prompt

**Files:**
- Modify: `src/onboarding.rs`

**Interfaces:**
- Consumes: `config::{SecretSource, resolve_shared_secret, persist_secret, generate_secret}`
  (P5-2); `add_passthrough_provider` (P5-5); `add_codex_provider` (P5-6);
  `assign_to_pool` (P5-4).
- Produces:
  ```rust
  /// The one entry point both triggers share.
  ///
  /// `sqlite_path` is needed to locate the secret sidecar. Returns the
  /// resolved secret so the first-boot caller can build its Config from it
  /// without re-reading the file.
  pub async fn run_wizard(
      db: &sqlx::SqlitePool,
      http: &reqwest::Client,
      sqlite_path: &str,
  ) -> anyhow::Result<String>;

  /// True when stdin is a terminal (gate for every interactive path).
  pub fn stdin_is_tty() -> bool;

  /// Providers table is empty.
  pub async fn providers_table_is_empty(db: &sqlx::SqlitePool) -> anyhow::Result<bool>;
  ```

- [ ] **Step 1: Write the failing test**

`run_wizard` itself is prompt-driven and covered by P5-9's manual smoke
checklist. The machine-testable part is the trigger predicate. Add to
`mod tests`:

```rust
    #[tokio::test]
    async fn providers_table_emptiness_predicate() {
        let db = init_pool(":memory:").await.unwrap();
        assert!(providers_table_is_empty(&db).await.unwrap());

        insert_provider(&db, &provider("p1", WireFormat::OpenAi)).await.unwrap();
        assert!(!providers_table_is_empty(&db).await.unwrap());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding`
Expected: FAIL — `cannot find function `providers_table_is_empty``.

- [ ] **Step 3: Write minimal implementation**

Add to `src/onboarding.rs`:

```rust
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

/// Resolve the admin secret, prompting to generate-or-enter one if none
/// exists yet, and persist it to the sidecar file.
///
/// Persisting is what lets a later `1router setup` skip this step entirely.
fn resolve_or_prompt_secret(sqlite_path: &str) -> anyhow::Result<String> {
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
                .items(&["Generate a random secret (recommended)", "Enter my own"])
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
            .items(&["passthrough (OpenAI/Anthropic-compatible API key)",
                     "Codex OAuth (ChatGPT account)"])
            .default(0)
            .interact()?;

        let provider = match kind {
            0 => add_passthrough_provider(db).await?,
            _ => add_codex_provider(db, http).await?,
        };

        // Pool id: what clients will send as `model`.
        let default_pool = provider.id.clone();
        let pool_id: String = Input::with_theme(&theme())
            .with_prompt("Pool id (this is the `model` name clients will request)")
            .default(default_pool)
            .interact_text()?;
        let pool_id = pool_id.trim().to_string();
        let priority = assign_to_pool(db, &pool_id, &provider).await?;
        println!(
            "  added '{}' to pool '{pool_id}' at priority {priority}",
            provider.id
        );

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
```

Note on Ctrl-C / EOF: `dialoguer`'s `interact*` returns
`Err(dialoguer::Error::IO)` on EOF and (with its default settings) exits the
process on Ctrl-C. Both propagate as an `Err` out of `run_wizard` via `?`,
which is exactly the spec's requirement — an interrupted wizard must not fall
through into serving traffic with config the operator never confirmed. Do not
add a `.unwrap_or_default()` anywhere in this function.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding && cargo build --offline`
Expected: PASS — 18 tests, clean build (the `Confirm` import from P5-5 is now
used).

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(onboarding): run_wizard - secret bootstrap, provider loop, pool prompt

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-8: Wire both triggers into `main.rs`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `onboarding::{run_wizard, stdin_is_tty, providers_table_is_empty}`
  (P5-7); `config::{sqlite_path_from_env, resolve_shared_secret, persist_secret,
  generate_secret, SecretSource, Config::from_env_with_secret}` (P5-2);
  `init_pool`, `build_client`, `seed_if_configured` (existing).
- Produces: two entry points —
  - `1router setup`: run the wizard, exit 0. Exit non-zero with a stderr
    message if stdin is not a TTY.
  - `1router` (normal boot): resolve the secret (auto-generating + logging once
    if there is none and there is no TTY), then run the wizard iff
    providers is empty **and** `ROUTER_SEED_PATH` is unset **and** stdin is a
    TTY, all before `load_snapshot`.

- [ ] **Step 1: Write the failing test**

`main()` is not unit tested anywhere in this crate (Phase 4's P4-1 established
that startup is covered by `tests/startup.rs` going through
`tests/common::spawn_app`, which builds the router directly and never calls
`main`). The verification for this task is `cargo build --offline`, the
unchanged full suite, and P5-9's manual smoke checklist. Do not invent an
integration test that shells out to the binary — it would need a PTY.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo build --offline`
Expected: currently PASSES (nothing written yet) — so instead, the "failing"
state to observe is behavioural. With a scratch dir and no
`ROUTER_SHARED_SECRET`:

```bash
cd "$(mktemp -d)" && ROUTER_SQLITE_PATH=./t.db "$OLDPWD/target/debug/1router" setup
```
Expected: FAIL — the binary ignores `setup` entirely and either errors with
`ROUTER_SHARED_SECRET is required` / the new no-secret message, or starts
serving. Neither is the wizard.

- [ ] **Step 3: Write minimal implementation**

Rewrite the top of `src/main.rs`'s `main()` (everything from `init_tracing()`
down to `let log_tx = ...` — leave the whole shutdown/serve section below it
untouched):

```rust
use anyhow::Result;
use std::sync::Arc;

use router::core::config::{self, Config, SecretSource};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{load_snapshot, AppState};
use router::onboarding;
use router::providers::refresh_task::spawn_background_refresh;
use router::seed::seed_if_configured;
use router::telemetry::logging::init_tracing;
use router::telemetry::request_log::spawn_writer;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let sqlite_path = config::sqlite_path_from_env();

    // Subcommand check first, before any other startup work. One deliberate
    // arg check instead of a CLI parser dependency (spec non-goal).
    if std::env::args().nth(1).as_deref() == Some("setup") {
        if !onboarding::stdin_is_tty() {
            eprintln!(
                "`1router setup` is interactive and needs a terminal on stdin. \
                 For scripted config, set ROUTER_SEED_PATH to a config JSON file instead."
            );
            std::process::exit(2);
        }
        let db = init_pool(&sqlite_path).await?;
        // build_client needs a Config, and a Config needs a secret - which the
        // wizard may be about to create. Use a plain client for the wizard's
        // own requests (only the OAuth exchange + model probes) rather than
        // ordering the two around each other.
        let http = reqwest::Client::new();
        onboarding::run_wizard(&db, &http, &sqlite_path).await?;
        return Ok(());
    }

    // Normal boot. Resolve the secret before anything can need it.
    let secret = match config::resolve_shared_secret(&sqlite_path)? {
        SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s),
        SecretSource::BootstrapNeeded if onboarding::stdin_is_tty() => None, // wizard will make one
        SecretSource::BootstrapNeeded => {
            // Headless first boot: auto-generate, persist, and log it ONCE.
            let s = config::generate_secret();
            config::persist_secret(&sqlite_path, &s)?;
            tracing::info!(
                secret = %s,
                path = ?config::secret_file_path(&sqlite_path),
                "generated a new admin shared secret - SAVE THIS NOW, it will not be logged \
                 again. Set ROUTER_SHARED_SECRET to control it explicitly."
            );
            Some(s)
        }
    };

    let db = init_pool(&sqlite_path).await?;
    seed_if_configured_first(&db, &sqlite_path).await?;

    // First-boot wizard: empty DB + no seed file + a real terminal. Any one of
    // those missing means "don't block a headless/scripted deployment".
    let seed_configured = std::env::var("ROUTER_SEED_PATH").is_ok();
    let secret = match secret {
        Some(s) => s,
        None => {
            // BootstrapNeeded + TTY: the wizard both creates the secret and
            // (optionally) the first provider, and hands the secret back.
            let http = reqwest::Client::new();
            onboarding::run_wizard(&db, &http, &sqlite_path).await?
        }
    };
    if !seed_configured
        && onboarding::stdin_is_tty()
        && onboarding::providers_table_is_empty(&db).await?
    {
        let http = reqwest::Client::new();
        onboarding::run_wizard(&db, &http, &sqlite_path).await?;
    }

    let cfg = Config::from_env_with_secret(secret)?;
    let http = build_client(&cfg);
    let snapshot = load_snapshot(&db).await?;
    let log_tx = spawn_writer(db.clone(), 4096, 100);
    // ... rest unchanged (AppState, spawn_background_refresh, serve) ...
}
```

Two ordering details worth getting right, both mandated by the spec:

- `seed_if_configured` takes a `&Config`, which needs the secret — but the
  secret may not exist yet when the wizard is what creates it. Keep the call
  where it is relative to the wizard (seed **before** wizard, so the
  "providers table is empty" check the wizard does reflects the seed's effect
  and `ROUTER_SEED_PATH` genuinely wins) by moving the seed to a small local
  helper that reads `ROUTER_SEED_PATH` itself:

  ```rust
  /// seed_if_configured needs a Config (for `seed_path`), but the secret may
  /// not exist yet at this point in boot. The seed only ever reads
  /// `cfg.seed_path`, so build a throwaway Config with a dummy secret for it.
  async fn seed_if_configured_first(db: &sqlx::SqlitePool, _sqlite_path: &str) -> Result<()> {
      let cfg = Config::from_env_with_secret(String::new())?;
      seed_if_configured(db, &cfg).await
  }
  ```
  Verify by reading `src/seed.rs`: `seed_if_configured` touches only
  `cfg.seed_path`, so the empty secret is never observable. If that ever stops
  being true, change `seed_if_configured` to take `Option<&Path>` instead.

- The two `run_wizard` call sites above are mutually exclusive in practice
  (the first branch already ran the full wizard, which includes the provider
  loop, so its `providers_table_is_empty` check is false by the time the second
  `if` is evaluated — unless the user answered "no" to "Add a provider now?",
  in which case re-prompting would be wrong). **Collapse them into one call**:
  track a `wizard_already_ran: bool` from the first branch and add
  `&& !wizard_already_ran` to the `if`. Do not ship the double-prompt.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo build --offline && cargo test --offline`
Expected: PASS — clean build, whole suite green (no test drives `main`).
Then the behavioural check from Step 2:

```bash
cd "$(mktemp -d)" && ROUTER_SQLITE_PATH=./t.db "$OLDPWD/target/debug/1router" setup
```
Expected: the wizard's `=== 1router setup ===` banner and the
generate-or-enter-a-secret prompt. Ctrl-C out; confirm exit is non-zero and
`./.router_secret` was **not** created (Ctrl-C before the choice) — then run it
again, pick "generate", and confirm `.router_secret` exists with mode 0600.

And the headless path:

```bash
cd "$(mktemp -d)" && ROUTER_SQLITE_PATH=./t.db "$OLDPWD/target/debug/1router" < /dev/null
```
Expected: one `info` log line containing the generated secret and the
save-this-now wording, `.router_secret` created, and the server proceeds to
`1router listening` with **no** interactive prompt.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: first-boot wizard trigger + `1router setup` subcommand

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task P5-9: Documentation + manual smoke-test checklist

**Files:**
- Modify: `README.md` (quickstart section referencing the wizard)
- Modify: `CLAUDE.md` (one line: the wizard exists; `dialoguer` is a dep)
- Create: `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
  (the checklist below, as a runnable doc)

**Interfaces:**
- Consumes: the finished feature.
- Produces: a quickstart a new user can follow, and a human-executable
  verification script for the parts no test covers.

- [ ] **Step 1: Write the failing test**

The "test" is that the README's quickstart, followed literally on a clean
machine, produces a working gateway. Write the checklist first (below), then
follow it.

- [ ] **Step 2: Run to verify it fails**

Read the current `README.md`. Expected: it documents only env vars and curl
calls; there is no mention of `1router setup`, of `.router_secret`, or of the
fact that `ROUTER_SHARED_SECRET` is now optional. That's the gap.

- [ ] **Step 3: Write minimal implementation**

Add to `README.md`, as the first thing after the intro:

```markdown
## Quickstart (interactive)

    1router setup

Walks you through: creating an admin secret (stored in `.router_secret` next to
the SQLite file, mode 0600), adding one provider — either a passthrough
OpenAI/Anthropic-compatible endpoint, or a Codex/ChatGPT account via OAuth (the
wizard probes which `upstream_model` your account accepts) — and putting it in
a pool. Then just run `1router`.

The same wizard runs automatically on first boot when the database is empty,
`ROUTER_SEED_PATH` is unset, and stdin is a terminal.

**Headless deployments** (Docker, systemd) never prompt: set
`ROUTER_SHARED_SECRET` (recommended) and `ROUTER_SEED_PATH` to a config JSON
file. If no secret is available at all, one is generated, saved to
`.router_secret`, and logged **once** at startup.

Secret resolution order: `ROUTER_SHARED_SECRET` → `.router_secret` sidecar →
generate one. A sidecar file that exists but can't be read is a fatal startup
error, never silently replaced.
```

Add to `CLAUDE.md` under "Build & test" or a new short "Onboarding" note:

```markdown
- `src/onboarding.rs` is the interactive wizard (`1router setup`, plus the
  first-boot auto-trigger). It is a thin `dialoguer` front end over
  `providers::queries` / `pools::queries` / `codex::oauth` — put no business
  logic in it. Its prompt paths are verified by
  `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`, not by
  `cargo test`. Design spec:
  `docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md`.
- `dialoguer` is a dependency as of Phase 5 — run `cargo fetch` with real
  network before any `--offline` work if your registry predates it.
```

Then create `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
with the checklist reproduced verbatim from the "Manual smoke test" section at
the end of this plan.

- [ ] **Step 4: Run to verify it passes**

Execute the manual smoke checklist below, top to bottom, ticking each box.
Then run the full automated suite one more time:

```bash
cargo build --offline
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add README.md CLAUDE.md docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md
git commit -m "docs: onboarding wizard quickstart + manual smoke checklist

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Manual smoke test (not automatable — a human must do this)

The `dialoguer` prompt sequences cannot be driven by `cargo test` (they read a
real terminal). Everything they *call* is already covered by unit/integration
tests; what needs a human is that the right prompts appear in the right order
and that the resulting DB rows are correct. Run every section below in a
**fresh empty directory** so `.router_secret` and the SQLite file start absent.

```bash
cargo build --offline
BIN="$PWD/target/debug/1router"
```

### A. `1router setup`, no secret, passthrough provider

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`
- [ ] Prompt appears: "No admin secret yet. Generate a random one, or enter your own?"
- [ ] Choose **Generate**. A 64-hex-char secret is printed once, plus the
      "will not be printed again" line.
- [ ] `ls -l .router_secret` → exists, `-rw-------` (0600), contents == the
      printed secret, no trailing newline issues (`wc -c` == 64).
- [ ] Prompt: "Add a provider now?" → **yes**.
- [ ] Prompt: "Provider kind" → **passthrough**.
- [ ] Enter name `smoke-openai`; wire format `openai`; base_url
      `https://api.openai.com/v1/chat/completions`; a real API key —
      **confirm the key is masked as you type**; upstream model `gpt-4o-mini`.
- [ ] Prompt: "Pool id" → accept the default (`smoke-openai`).
- [ ] Output confirms `added 'smoke-openai' to pool 'smoke-openai' at priority 1`.
- [ ] Prompt: "Add another provider?" → **no**. The example `curl` is printed
      and the process exits **0** (`echo $?`).
- [ ] `sqlite3 t.db 'select id,name,kind,wire_format,upstream_model from providers; select * from pools; select * from pool_members;'`
      → one passthrough provider, one pool, one member at priority 1.
      **`api_key` is the real key** (it is stored plaintext by design) but was
      never echoed to the terminal.
- [ ] Start the server in the same dir: `ROUTER_SQLITE_PATH=./t.db "$BIN"` —
      it boots **without prompting** (secret comes from the sidecar, providers
      table is non-empty) and logs `1router listening`.
- [ ] A real request works:
      `curl -s localhost:8080/v1/chat/completions -H "Authorization: Bearer $(cat .router_secret)" -H 'content-type: application/json' -d '{"model":"smoke-openai","messages":[{"role":"user","content":"Say OK"}]}'`
      → HTTP 200 with a `choices[0].message.content`.
- [ ] The same request with a wrong bearer → 401.

### B. Re-running `setup` on an already-configured install

- [ ] In the same directory, `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`.
- [ ] It does **not** re-ask for a secret; it prints "reusing ./.router_secret".
- [ ] Prompt reads "This gateway already has providers. Add another one?".
- [ ] Answer **yes**, add a second passthrough provider, and give it the
      **same pool id** as in section A.
- [ ] Output says priority **2** (not 1) — it went behind the incumbent.
- [ ] `sqlite3 t.db 'select * from pool_members order by priority;'` confirms
      1 then 2, and the pool row's `created_at` is unchanged.
- [ ] Answer **no** to "Add another"; exit code 0.

### C. `ROUTER_SHARED_SECRET` wins over the sidecar

- [ ] Still in the same dir: `ROUTER_SQLITE_PATH=./t.db ROUTER_SHARED_SECRET=env-wins "$BIN" setup`
- [ ] It prints "using ROUTER_SHARED_SECRET from the environment" and does not
      touch `.router_secret` (`ls -l` mtime unchanged; contents unchanged).
- [ ] Ctrl-C at the next prompt; exit code is non-zero.

### D. Corrupt sidecar is fatal, not silently replaced

- [ ] `chmod 000 .router_secret` then `ROUTER_SQLITE_PATH=./t.db "$BIN"`
      (as a non-root user).
- [ ] Startup **fails** with a "failed to read secret file" error naming the
      path. Exit code non-zero.
- [ ] `.router_secret` still holds the original secret (nothing regenerated).
- [ ] `chmod 600 .router_secret` restores normal boot.
- [ ] `printf '' > .router_secret` → startup fails with the "is empty" message.
      Restore the real secret afterwards.

### E. First-boot auto-trigger with a TTY

- [ ] `cd "$(mktemp -d)"` (fresh) then `ROUTER_SQLITE_PATH=./t.db "$BIN"` —
      **no `setup` argument**.
- [ ] The wizard runs automatically (empty DB + no seed path + TTY), starting
      with the secret prompt.
- [ ] Complete it with one passthrough provider; after "Add another? → no",
      the process **continues into normal startup** and logs
      `1router listening` (it does not exit).
- [ ] Ctrl-C to stop the server.
- [ ] Repeat `ROUTER_SQLITE_PATH=./t.db "$BIN"` in the same dir: **no wizard**
      (providers table non-empty), straight to listening.

### F. First-boot auto-trigger is suppressed by `ROUTER_SEED_PATH`

- [ ] `cd "$(mktemp -d)"` (fresh). Write a minimal seed file:
      `echo '{"providers":[{"id":"seeded","name":"seeded","wire_format":"openai","kind":"passthrough","base_url":"https://x/v1/chat/completions","api_key":"k","upstream_model":"m","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}],"pools":[],"members":[]}' > seed.json`
- [ ] `ROUTER_SQLITE_PATH=./t.db ROUTER_SEED_PATH=./seed.json ROUTER_SHARED_SECRET=x "$BIN"`
- [ ] **No wizard prompt at all**, even though stdin is a TTY. Logs
      `first-boot seed applied` then `1router listening`.
- [ ] Same again but **without** `ROUTER_SHARED_SECRET` in a fresh dir: still
      no wizard; a secret is generated, logged once, and written to
      `.router_secret`.

### G. No-TTY paths never block

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" < /dev/null`
- [ ] No prompt. One `info` log line with the generated secret and the
      "SAVE THIS NOW" wording. `.router_secret` created at 0600. Server reaches
      `1router listening` with an empty provider set.
- [ ] `curl -s localhost:8080/health` → 200 (health is unauthenticated).
- [ ] `curl -s localhost:8080/v1/models -H "Authorization: Bearer $(cat .router_secret)"`
      → 200 with an empty list, proving the logged secret is the live one.
- [ ] Ctrl-C. Then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup < /dev/null`
      → prints the "needs a terminal on stdin" message to **stderr** and exits
      with status **2**. It does **not** hang.

### H. Codex OAuth provider (needs a real ChatGPT account + a browser)

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`;
      generate a secret; add a provider; choose **Codex OAuth**.
- [ ] Enter name `smoke-codex`. The authorize URL is printed with the
      three-step instructions.
- [ ] **Paste garbage** at the prompt first → it reports "could not find both
      `code` and `state`" and **re-prompts** (the wizard does not abort and does
      not make you redo the authorize step).
- [ ] Open the URL, log in, copy the `localhost:1455/auth/callback?...` URL
      from the address bar (it will fail to load — expected) and paste it.
- [ ] "login stored." then "Probing which model this ChatGPT account
      accepts..." with one `trying "<model>"` line per candidate, in the order
      `gpt-5.4, gpt-5-codex, gpt-5.1-codex, gpt-5, codex-mini-latest`,
      **stopping at the first success**.
- [ ] `-> using upstream_model "<model>"` is printed and
      `sqlite3 t.db 'select upstream_model from providers;'` matches it (not
      `pending`).
- [ ] Assign it to pool `smoke-codex`; finish the wizard.
- [ ] `sqlite3 t.db 'select provider_id, access_token is not null, refresh_token is not null, pkce_verifier is null, oauth_state is null from provider_oauth_state;'`
      → tokens present, PKCE columns cleared.
- [ ] Start the server and send a real chat completion against pool
      `smoke-codex` → HTTP 200 with content. This is the end-to-end proof.
- [ ] Also paste an **already-used** code on a second run to confirm the
      exchange failure re-prompts in place rather than aborting.

### I. Model-probe total failure is not fatal

Hard to force naturally; simulate it by temporarily editing
`CANDIDATE_MODELS` to a single bogus value (`["definitely-not-a-model"]`),
rebuilding, and re-running section H.

- [ ] Every attempt's status + body is printed.
- [ ] The wizard **continues** to the pool prompt (it does not abort).
- [ ] `upstream_model` stays `pending` in the DB, and the printed hint shows
      the exact `PATCH /admin/providers/smoke-codex` curl to fix it.
- [ ] Revert the `CANDIDATE_MODELS` edit and rebuild before committing
      anything. **Do not commit the bogus list.**

---

## Final verification checklist

```bash
cargo build --offline --release
cargo test --offline                     # all unit + integration; e2e stay ignored
cargo clippy --offline --all-targets -- -D warnings
```

- [ ] All of the above green.
- [ ] `git diff --stat` touches only: `Cargo.toml`, `Cargo.lock`,
      `src/onboarding.rs`, `src/lib.rs`, `src/core/config.rs`, `src/main.rs`,
      `src/providers/oauth_routes.rs`, `README.md`, `CLAUDE.md`, and the two
      docs files. **Anything else means scope creep — justify or revert it.**
- [ ] `grep -rn "shared_secret\|ROUTER_SHARED_SECRET" src/` shows no new
      logging of the secret value other than the one deliberate no-TTY
      bootstrap `info!` in `main.rs`.
- [ ] The manual smoke checklist above is fully ticked, including section H
      against a real ChatGPT account.
- [ ] `CANDIDATE_MODELS` in `src/onboarding.rs` is identical to
      `candidate_models` in `tests/e2e_real_providers.rs` (the unit test
      `candidate_list_matches_the_e2e_test` pins one side; eyeball the other).
- [ ] Existing behaviour is unchanged for anyone who sets
      `ROUTER_SHARED_SECRET`: `tests/startup.rs`, `tests/codex_oauth.rs` and
      the admin tests all pass untouched.
