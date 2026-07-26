# 1router Admin UI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a small embedded React admin SPA to `1router` on top of the existing `/admin/*` API — providers/pools CRUD, pool member reordering, Codex OAuth guided flow, connectivity testing — gated by its own session-based login, fully separate from the shared-secret Bearer auth that continues to protect `/v1/*` proxy traffic unchanged.

**Source spec:** `docs/superpowers/specs/2026-07-26-admin-ui-design.md` (already twice-reviewed and revised for security/correctness gaps before this plan was written).

**How this plan was produced:** a 4-stage multi-agent pipeline — a Technical Architect (Opus) drafted the phase/task decomposition, a Senior Rust Engineer (Opus) added concrete TDD detail, four Codex agents in parallel drafted full TDD-step content per subsystem (backend auth, shared-secret settings, frontend, build/CI/Docker), and a final Opus review pass audited the assembled result for consistency, completeness, and correctness against the actual codebase. The review caught several real bugs before implementation started (see "Assembly & review notes" at the end of this document) — they are fixed inline below, not left as follow-up work.

**Architecture:** Same single-binary axum/tokio/sqlx crate. New `frontend/` (React+Vite+TS) directory built via `npm run build`, embedded into the binary at compile time via `rust-embed` behind a default-on `ui` Cargo feature, served at `/ui/*path`. New `admin_users`/`admin_sessions`/`server_secrets` SQLite tables. `/admin/*` splits into an unauthenticated stratum (login only) and a session-authenticated stratum (everything else, with a Bearer-secret fallback so existing curl/CI usage doesn't break); `/v1/*` is untouched.

## Global constraints (in addition to the base project's, `CLAUDE.md`)

- Everything in the original plan's Global Constraints section still applies (axum 0.7 `:id` route syntax, `sqlx::migrate!` for all schema, structured JSON logs, secret redaction, feature-first modules).
- **Two separate credentials, never merged:** the admin-UI login (`admin_users`, session cookies) and the shared secret (`/v1/*` Bearer auth) are independent systems end to end — different tables, different endpoints, different middleware. Do not let implementation convenience blur this boundary.
- **`require_admin_session` accepts cookie OR Bearer, never neither.** This is what keeps existing curl/CI usage of `/admin/*` working unchanged (spec's non-goal: export/import stay CLI/curl-usable).
- **The `ui` Cargo feature is default-on once D2 lands.** Every `cargo build --offline`/`cargo test --offline` invocation after that point — including anything dispatched into a Codex worktree per this project's existing orchestration pattern — must add `--no-default-features`, or `build.rs`'s `npm` shellout fails fast in a network-less sandbox. (Task E4 below adds this permanently to `CLAUDE.md`.)
- **`ConnectInfo<SocketAddr>` must be wired at both `axum::serve` call sites** (`src/main.rs` and `tests/common::spawn_app`) in the same commit (Task B2) — missing one silently compiles but the rate limiter never sees real client IPs there.

## 0. Shared-file collision map (read this before dispatching anything)

These are the files more than one task below touches. Per `CLAUDE.md`'s orchestration pattern, tasks that share a file **cannot run in the same wave of parallel worktrees** — they must be sequenced, and even then expect a hand-merge.

| File | Touched by | Why |
|---|---|---|
| `Cargo.toml` | A2, D2 (feature-gated deps), C1/C4/C5's `frontend/package.json` is a *separate* file, not this one — no cross-language collision | New Rust deps (`argon2`, `rust-embed`) land once in A2; D2 adds the `ui` feature block on top of A2's base — sequential, not parallel. |
| `src/core/state.rs` (`AppState`) | A4 (adds `shared_secret` field), B2 (adds `login_attempts` field) | Every hand-built `AppState` literal in the codebase (`src/app.rs` tests, `src/providers/refresh_task.rs` tests, `tests/common/mod.rs`) must be updated in lockstep with each field addition — exactly the "join point" problem `CLAUDE.md` already calls out for `AppState`/`app.rs`/`pools`/`telemetry`/`providers` `mod.rs` files. **A4 and B2 must run solo, one after another, never in parallel with each other or with anything else that constructs `AppState`.** |
| `src/auth/middleware.rs` | A4 (`require_bearer` reads `shared_secret.load()`), B4 (`require_admin_session`), B5 (CSRF guard) | B4 and B5 both land here after A4. They can be drafted as separate functions in parallel worktrees (no line overlap) but the merge is manual since it's one file — treat as sequential unless the two engineers pre-agree on non-overlapping insertion points. |
| `src/main.rs` | A4 (state field init), A6 (admin bootstrap block), B2 (`into_make_service_with_connect_info` at the `axum::serve` call + new `AppState` field init), B6 (spawn session-cleanup task) | Highest-collision file in this plan. Sequence strictly: A4 → A6 → B2 → B6. Do not parallelize any two of these. |
| `tests/common/mod.rs` | A4, B2 | Same reason as `AppState` above — every field addition breaks this test-harness literal. Whoever lands A4/B2 fixes this file as part of that same task, not as a follow-up. |
| `src/admin/mod.rs` (post-A3) | A3 (created), B3 (`pub mod auth;` + route merge), B7 (`pub mod settings;` + route merge) | Small, easily hand-mergeable collisions (same shape as the existing `pools/mod.rs`/`providers/mod.rs` pattern `CLAUDE.md` already tolerates). Not a blocker, just expect a 2-line merge conflict. |
| `src/app.rs` | A4 (test helper only), **E1** (the real router-stratification rewrite) | A4's touch is cosmetic (test literal). E1 is the true join task — it is the *only* task allowed to change `build_router`'s actual routing/layering, and it must run **after every other task in Phases A/B/D has landed**, solo, in its own worktree, last. |
| `frontend/package.json` | C1 (created), C5 (adds `dnd-kit`) | Same low-risk hand-merge pattern as `Cargo.toml`/`pools/mod.rs`. |

**The one true "P0-9/P0-10"-style join point in this whole plan is Task E1.** Everything in Phases A, B, and D produces a `routes()` function, a middleware function, or a spawn-a-task function with an agreed signature; E1 is where all of those get merged into `build_router` for real, and it cannot be parallelized with anything that also touches `src/app.rs`.

**Operational gotcha to flag for whoever dispatches this via the existing Codex-rescue pattern:** once **D2** lands, the `ui` Cargo feature is **default-on**. From that point forward, every task's `cargo build --offline` / `cargo test --offline` verification step (and the `PROMPT.md` template CLAUDE.md's orchestration section uses) must add `--no-default-features` — the Codex sandbox has neither network nor a `node`/`npm` binary, so `build.rs`'s shellout (D1) will fail fast otherwise. This mirrors the existing "Codex sandbox limitations" section in `CLAUDE.md` and should be added to it verbatim once D1/D2 land.

**Second gotcha, net-new (not in `CLAUDE.md` yet):** per-source-IP login rate limiting (B2) needs the caller's real IP, which axum only gives you via `ConnectInfo<SocketAddr>` — which in turn requires switching `axum::serve(listener, router)` to `axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())` (or an equivalent extractor) at **both** call sites that currently do a bare `axum::serve(listener, router)`: `src/main.rs` and `tests/common::spawn_app`. This is a real trap of the same shape as the axum 0.7 `:id`-vs-`{id}` gotcha already in `CLAUDE.md` — silently compiles wrong/differently rather than failing loudly if missed in one of the two spots. B2 must change both call sites in the same commit.

---

> **Post-review correction:** the table above (and the original A4/B2 task drafts) omitted 3 of the 7
> real `AppState { .. }` hand-built literal sites in this codebase. The verified complete set, confirmed
> via `grep -rln "AppState {" src tests`, is: `src/app.rs`, `src/main.rs`, `tests/common/mod.rs`,
> `tests/health_stats.rs`, `tests/admin_pools.rs`, `src/providers/refresh_lock.rs`,
> `src/providers/refresh_task.rs`. A4 and B2 (below) both carry the corrected file lists; this note
> exists so the table above isn't silently wrong for a reader who doesn't reach the task detail.

---

## Corrections made to the architect's decomposition

These are genuine gaps/mistakes found while grounding the plan in the actual code, not stylistic changes. Each is called out again inline at its task.

1. **B2 is missing two files.** The collision map says `AppState` gets its "second and last" field addition in B2, and correctly lists `tests/common/mod.rs` as a file B2 must fix — but it does **not** list `src/app.rs`'s `test_state()` helper or `src/providers/refresh_task.rs`'s `state_with()` helper. Both are hand-built `AppState { .. }` struct literals (no `..Default::default()`) that will fail to compile the instant a new field is added, exactly the failure mode the collision map itself warns about for A4. **Fixed:** B2's Files list now includes all three literal sites, mirroring A4's own file list exactly.
2. **B7's requested `AppState` field is renamed and given a smaller type.** The architect's write-up suggests folding `pub shared_secret_source: SecretSource` into A4. `core::config::SecretSource` carries the actual secret *value* (`Env(String)` / `SidecarFile(String)`), which would mean `AppState` holds a second, potentially-stale copy of the secret string that never gets updated by a live `PATCH` (since `PATCH` only calls `.store()` on `shared_secret`, not on this field). Fixed: A4 introduces a new, minimal `core::state::SecretOrigin { Env, SidecarFile }` enum — no payload, just tracks *where* the boot-time value came from, which is all B7's `409` check needs.
3. **`build.rs`'s feature detection is wrong as specified.** Cargo build scripts do **not** see the parent crate's active features via `cfg!(feature = "ui")` — that macro only reflects `build.rs`'s own compilation unit. Cargo instead exports `CARGO_FEATURE_UI=1` as an env var to the build script when the `ui` feature is active. Using `cfg!()` here would silently always take one branch regardless of the feature flag — a real "compiles wrong, not loudly" trap of the same shape as the axum `:id`/`{id}` gotcha. Fixed in D1: use `std::env::var("CARGO_FEATURE_UI").is_ok()`.
4. **B1 needs a function the architect's prose implies but never lists.** B3's password-change handler needs "delete all sessions except the one making this request" — B1's Interfaces list only names `delete_all_sessions(db)`. Fixed: B1 adds `delete_all_sessions_except(db, keep_token_hash)`.
5. **A cross-task type contract is missing.** B3's password-change handler needs to know *which* session is making the request (to exclude it from the invalidation sweep) and B4's middleware is the only place that has already validated the session. Fixed: B1 defines `pub struct AdminSession { pub token_hash: String }`; B4 inserts it into `Request::extensions_mut()` after a successful cookie validation; B3 extracts it via `Extension<AdminSession>`.
6. **`__Host-` cookie prefix trigger is undefined.** 1router never terminates TLS itself (confirmed — no TLS code anywhere in the codebase; it's designed to sit behind a reverse proxy, per the Docker single-port story). "When served over TLS" has to mean "the proxy told us via a header," not "axum has a TLS listener." Fixed: B1/B3/B4 use `X-Forwarded-Proto: https` (case-insensitive) as the signal; documented as a deployment requirement (the proxy must set this header consistently) rather than left to the implementer to discover mid-task.
7. **E2 needs one more thing than "reconciliation."** `tests/common::spawn_app` has no path to a known admin username/password (the real bootstrap flow, A6, is TTY/headless-branching and unsuitable for tests). E3's integration test (login → cookie → authenticated call) cannot exist without it. Fixed: E2 adds deterministic admin-user seeding to `spawn_app_with_sqlite_path` and a new `TestApp.admin_password` field.
8. **D2's rust-embed MIME lookup.** A2 only adds `rust-embed` (with its `mime-guess` *feature flag*), not a standalone `mime_guess` crate dependency — so D2 must use `EmbeddedFile::metadata.mimetype()` (rust-embed's own feature-gated method), not a separate `mime_guess::from_path(..)` call, which would need an undeclared dependency. Flagged inline in D2 as a small doc-verification item (exact method name/return type should be checked against the installed rust-embed 8.x docs at implementation time — this is a lookup, not a design decision).
9. **Session sliding-TTL length is undefined by the spec** (only the 7-day *absolute* cap is given as "e.g."). Picked concretely in B1: a 24-hour sliding window, renewed when under 50% remaining (< 12h left), capped at a 7-day absolute lifetime from `created_at`. Called out as a chosen value, not inferred from the spec.
10. **B2's "rolling window" is given a concrete, testable shape.** The spec's prose ("5 failures in a rolling window ... reset on success") is ambiguous between a time-boxed sliding window and a simple reset-on-success counter. The architect's own instruction to mirror `proxy::backoff`'s shape settles this: B2 uses a persistent per-IP failure counter (no time-decay other than the lockout duration itself), reset only on success — exactly like `ProviderRuntimeState`, just keyed by `IpAddr`.

---

## Phase A — Backend auth foundation

**Parallelism:** A1, A2, A3 are three-way leaf-parallel (disjoint files). A4 must run solo and should land before A5/A6 start. A5 depends on A2 (argon2) + A3 (module path) and is parallel-safe with A4 (disjoint files). A6 depends on A1 + A5 and must land after A4 (both touch `main.rs`).

### Task A1: `admin_ui` migration

**Files:**
- Create: `migrations/0002_admin_ui.sql`

**Interfaces:**
- Consumes: nothing.
- Produces: `admin_users`, `admin_sessions` plus `idx_admin_sessions_expires`, and `server_secrets`, picked up automatically by `sqlx::migrate!("./migrations")` in `src/core/db.rs::init_pool`.

- [ ] **Step 1: Write the failing test**

No Rust test. Validate the migration file as SQL with `sqlite3`. Before implementation, the file is intentionally absent.

- [ ] **Step 2: Run to verify it fails**

```bash
rm -f /tmp/1router-admin-ui-schema-check.db
sqlite3 /tmp/1router-admin-ui-schema-check.db < migrations/0002_admin_ui.sql && echo OK
```

Expected: FAIL — shell reports `migrations/0002_admin_ui.sql: No such file or directory`.

- [ ] **Step 3: Write minimal implementation**

Create `migrations/0002_admin_ui.sql`:

```sql
CREATE TABLE admin_users (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    username TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE admin_sessions (
    token_hash TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_admin_sessions_expires ON admin_sessions(expires_at);

CREATE TABLE server_secrets (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

- [ ] **Step 4: Run to verify it passes**

```bash
rm -f /tmp/1router-admin-ui-schema-check.db
sqlite3 /tmp/1router-admin-ui-schema-check.db < migrations/0002_admin_ui.sql && echo OK
sqlite3 /tmp/1router-admin-ui-schema-check.db ".tables"
cargo test --offline --lib core::db::tests::init_pool_applies_migrations_and_wal
```

Expected: PASS — first command prints `OK`; `.tables` includes `admin_sessions  admin_users  server_secrets`; cargo output includes:

```text
test core::db::tests::init_pool_applies_migrations_and_wal ... ok
test result: ok. 1 passed
```

- [ ] **Step 5: Commit**

```bash
git add migrations/0002_admin_ui.sql
git commit -m "feat: admin_ui schema migration (users/sessions/server_secrets)"
```
### Task A2: New Cargo dependencies + `ui` feature skeleton

**Files:** Modify `Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `argon2` (unconditional — login exists even with `ui` off) and the feature plumbing D2 will write code against.

```toml
[dependencies]
# ... existing deps unchanged ...
argon2 = "0.5"
rust-embed = { version = "8", optional = true, features = ["mime-guess"] }

[features]
default = ["ui"]
ui = ["dep:rust-embed"]
```

Deliberate non-decisions, called out so downstream tasks don't re-litigate them:
- **No cookie-jar crate.** Cookie directives needed are `HttpOnly`/`Secure`/`SameSite=Strict`/optional `__Host-` prefix/`Max-Age` — a few lines of string building/splitting. B1/B3/B4 hand-roll this (matches the codebase's existing no-`axum-extra` style). Do **not** add `axum-extra` or `cookie` — that would be a second, avoidable `Cargo.toml` collision.
- **No separate RNG crate.** `argon2`'s `password-hash` re-export (rand_core 0.6) is compatible with the existing `rand = "0.8"` dependency's `OsRng` — A5 uses `rand::rngs::OsRng` directly.

- [ ] **Step 1/2:** No test; verify via `cargo build --offline` (expect FAIL before dep is fetched — see below) then after `cargo fetch` (real network) + rebuild, PASS.
- [ ] **Step 3:** Diff above.
- [ ] **Step 4:** **After this task lands, run `cargo fetch` with real network** (same note as the existing `dialoguer` precedent in `CLAUDE.md`) before dispatching any later task into a Codex worktree. Then `cargo build --offline` and `cargo build --offline --no-default-features` both succeed (the second proves `ui`/`rust-embed` compiles out cleanly with zero code yet depending on it).
- [ ] **Step 5:** `git add Cargo.toml Cargo.lock && git commit -m "feat: add argon2 dep and ui feature skeleton (rust-embed optional)"`
### Task A3: Convert `src/admin.rs` → `src/admin/mod.rs`

**Files:** Modify (move) `src/admin.rs` → `src/admin/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a module directory, the only way A5/B3/B7 can add `pub mod auth;`/`pub mod settings;`. `src/lib.rs`'s `pub mod admin;` needs no change.

- [ ] **Step 1/2:** No behavior change, so no new test. Verify via `git mv src/admin.rs src/admin/mod.rs` then `cargo test --offline --lib admin` — must still pass with **identical** test output to before the move (same test names: `admin::tests::import_is_all_or_nothing_on_failure`).
- [ ] **Step 3:** `git mv src/admin.rs src/admin/mod.rs` — no content edits at all.
- [ ] **Step 4:** `cargo build --offline` and `cargo test --offline --lib admin` both pass, zero diff in test names/count vs. before the move.
- [ ] **Step 5:** `git add -A && git commit -m "refactor: convert src/admin.rs to src/admin/mod.rs (module directory, no behavior change)"`
### Task A4: `AppState.shared_secret` live secret handle + `SecretOrigin`

**Corrections grounded in current code:**
- `.pi/ADMIN_UI_ENGINEERING_PLAN.md` is not present in this checkout, so this task detail reconstructs A4 strictly from the requested A4 scope, the admin UI design spec, the reference plan format, and current source.
- Current code already has `core::config::SecretSource`, not `SecretOrigin`. A4 should add a separate lightweight `SecretOrigin` enum to `core::state` because runtime state needs only precedence/origin metadata, not another copy of the secret-bearing `SecretSource`.
- `AppError::Conflict`, `AppError::BadRequest`, `AppError::Unauthorized`, and `AppError::Internal` already exist in `src/core/error.rs`; A4 does not need to create them.
- Current admin routing lives in `src/admin.rs`, not `src/admin/mod.rs`.

**Files:**
- Modify: `src/core/state.rs`
- Modify: `src/auth/middleware.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`
- Modify: `src/providers/refresh_task.rs`
- Modify: `src/providers/refresh_lock.rs`
- Modify: `tests/common/mod.rs`
- Modify: `tests/admin_pools.rs`
- Modify: `tests/health_stats.rs`

**Interfaces:**
- Consumes:
  - `crate::core::config::SecretSource`
  - `arc_swap::ArcSwap<String>`
  - Existing `AppState` construction sites
  - Existing `require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response`
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
  #[serde(rename_all = "snake_case")]
  pub enum SecretOrigin {
      Env,
      SidecarFile,
  }

  impl SecretOrigin {
      pub fn from_source(source: &crate::core::config::SecretSource) -> Option<Self>;
  }

  pub struct AppState {
      pub db: sqlx::SqlitePool,
      pub http: reqwest::Client,
      pub config: std::sync::Arc<crate::core::config::Config>,
      pub shared_secret: std::sync::Arc<arc_swap::ArcSwap<String>>,
      pub secret_origin: SecretOrigin,
      pub snapshot: std::sync::Arc<arc_swap::ArcSwap<ConfigSnapshot>>,
      pub runtime: RuntimeStateMap,
      pub log_tx: RequestLogSender,
      pub refresh_locks: RefreshLocks,
  }

  pub async fn require_bearer(
      State(state): axum::extract::State<AppState>,
      req: axum::extract::Request,
      next: axum::middleware::Next,
  ) -> axum::response::Response;
  ```

- [ ] **Step 1: Write the failing test**

Add this test to `src/app.rs` inside the existing `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn bearer_auth_reads_live_shared_secret_not_config_copy() {
    let state = test_state().await;
    state
        .shared_secret
        .store(Arc::new("rotated-secret".to_string()));

    let router = build_router(state.clone());

    let old = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/stats")
                .header("authorization", "Bearer s")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED);

    let new = router
        .oneshot(
            Request::builder()
                .uri("/admin/stats")
                .header("authorization", "Bearer rotated-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(new.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run to verify it fails**

Run:

```bash
cargo test --offline bearer_auth_reads_live_shared_secret_not_config_copy
```

Expected: FAIL at compile time because `AppState` does not yet expose a live shared-secret handle:

```text
error[E0609]: no field `shared_secret` on type `state::AppState`
```

- [ ] **Step 3: Write minimal implementation**

In `src/core/state.rs`, add `SecretOrigin` and the two new `AppState` fields:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretOrigin {
    Env,
    SidecarFile,
}

impl SecretOrigin {
    pub fn from_source(source: &crate::core::config::SecretSource) -> Option<Self> {
        match source {
            crate::core::config::SecretSource::Env(_) => Some(SecretOrigin::Env),
            crate::core::config::SecretSource::SidecarFile(_) => Some(SecretOrigin::SidecarFile),
            crate::core::config::SecretSource::BootstrapNeeded => None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    pub shared_secret: Arc<ArcSwap<String>>,
    pub secret_origin: SecretOrigin,
    pub snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    pub runtime: RuntimeStateMap,
    pub log_tx: RequestLogSender,
    pub refresh_locks: RefreshLocks,
}
```

In `src/auth/middleware.rs`, read the mutable runtime secret instead of the immutable config copy:

```rust
pub async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let current_secret = state.shared_secret.load();
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == current_secret.as_str())
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "unauthorized" } })),
        )
            .into_response()
    }
}
```

In `src/main.rs`, preserve the resolved origin before consuming the secret:

```rust
let resolved_secret = config::resolve_shared_secret(&sqlite_path)?;
let mut secret_origin = router::core::state::SecretOrigin::from_source(&resolved_secret);

let secret = match resolved_secret {
    SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s),
    SecretSource::BootstrapNeeded if onboarding::stdin_is_tty() => {
        let s = onboarding::resolve_or_prompt_secret(&sqlite_path)?;
        secret_origin = Some(router::core::state::SecretOrigin::SidecarFile);
        Some(s)
    }
    SecretSource::BootstrapNeeded => {
        let s = config::generate_secret();
        config::persist_secret(&sqlite_path, &s)?;
        tracing::info!(
            secret = %s,
            path = ?config::secret_file_path(&sqlite_path),
            "generated a new admin shared secret - SAVE THIS NOW, it will not be logged \
             again. Set ROUTER_SHARED_SECRET to control it explicitly."
        );
        secret_origin = Some(router::core::state::SecretOrigin::SidecarFile);
        Some(s)
    }
};
let secret = secret.expect("all resolve_shared_secret arms above produce a secret");
let secret_origin = secret_origin.expect("all resolved runtime secrets have an origin");
```

Then add the fields when building `AppState` in `src/main.rs`:

```rust
let state = AppState {
    db,
    http,
    config: Arc::new(cfg.clone()),
    shared_secret: Arc::new(arc_swap::ArcSwap::from_pointee(secret.clone())),
    secret_origin,
    snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(snapshot)),
    runtime: Arc::new(dashmap::DashMap::new()),
    log_tx,
    refresh_locks: Arc::new(dashmap::DashMap::new()),
};
```

Update every test/helper `AppState` initializer in `src/app.rs`, `src/providers/refresh_task.rs`, `src/providers/refresh_lock.rs`, `tests/common/mod.rs`, `tests/admin_pools.rs`, and `tests/health_stats.rs` with:

```rust
shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
secret_origin: router::core::state::SecretOrigin::SidecarFile,
```

Use `crate::core::state::SecretOrigin::SidecarFile` instead of `router::...` inside crate-local unit tests under `src/`.

Where tests currently call `auth_header(&state.config.shared_secret)`, prefer the live value so the tests track the new source of truth:

```rust
let secret = state.shared_secret.load();
let (k, v) = auth_header(secret.as_str());
```

- [ ] **Step 4: Run to verify it passes**

Run:

```bash
cargo test --offline bearer_auth_reads_live_shared_secret_not_config_copy
```

Expected: PASS:

```text
running 1 test
test app::tests::bearer_auth_reads_live_shared_secret_not_config_copy ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Also run the broader auth/admin surface because this field touches shared test helpers:

```bash
cargo test --offline --test admin_pools --test health_stats
```

Expected: PASS, with the existing integration tests passing:

```text
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

- [ ] **Step 5: Commit**

```bash
git add src/core/state.rs src/auth/middleware.rs src/main.rs src/app.rs src/providers/refresh_task.rs src/providers/refresh_lock.rs tests/common/mod.rs tests/admin_pools.rs tests/health_stats.rs
git commit -m "feat: make shared secret live reloadable

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```
### Task A5: Password hashing module

**Files:** Create `src/admin/auth/mod.rs` (`pub mod password;`), Create `src/admin/auth/password.rs`

**Interfaces:**
- Consumes: `argon2` (A2).
- Produces:
  ```rust
  pub fn hash_password(plain: &str) -> anyhow::Result<String>
  pub fn verify_password(hash: &str, plain: &str) -> bool
  ```

```rust
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;

/// Argon2id via the crate's built-in default params (RFC-9106-recommended
/// low-memory profile) — deliberate, not hand-tuned: resolves the spec's open
/// question rather than leaving it for later.
pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))
}

/// Constant-time by construction (PasswordVerifier). Never panics on a
/// malformed `hash` string — returns false instead, since callers pass
/// untrusted DB content through here.
pub fn verify_password(hash: &str, plain: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else { return false };
    Argon2::default().verify_password(plain.as_bytes(), &parsed).is_ok()
}
```

Tests in `password.rs`:
- `hash_and_verify_round_trip` — `hash_password("correct horse")` then `verify_password(&hash, "correct horse")` is `true`.
- `verify_rejects_wrong_password` — `verify_password(&hash, "wrong")` is `false`.
- `hash_is_randomized_per_call` — two hashes of the same plaintext differ (salted).
- `verify_rejects_malformed_hash_string_without_panicking` — `verify_password("not-a-real-hash", "x")` is `false`.

- [ ] **Step 2:** `cargo test --offline --lib admin::auth::password` → FAIL (no such module).
- [ ] **Step 4:** same command → PASS, 4 tests.
- [ ] **Step 5:** `git add src/admin/auth/mod.rs src/admin/auth/password.rs && git commit -m "feat: argon2 password hashing module"`
### Task A6: Admin bootstrap TTY-vs-headless dual path

**Files:**
- Modify: `src/onboarding.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: A1 `admin_users`, A5 `crate::admin::auth::password::hash_password`, existing `onboarding::stdin_is_tty()`, existing `core::config::generate_secret()`.
- Produces:
  ```rust
  pub async fn resolve_or_prompt_admin_password(db: &sqlx::SqlitePool) -> anyhow::Result<()>
  ```
  guaranteeing a non-empty single admin row before the server accepts connections.

- [ ] **Step 1: Write the failing test**

Add to `src/onboarding.rs` inside the existing `#[cfg(test)] mod tests` or create it if missing:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --offline --lib onboarding::admin_bootstrap_tests
```

Expected: FAIL — compiler reports `cannot find function resolve_or_prompt_admin_password in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/onboarding.rs` near `resolve_or_prompt_secret`:

```rust
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
```

Modify `src/main.rs`, immediately after `seed_if_configured_first(&db).await?;`:

```rust
seed_if_configured_first(&db).await?;
onboarding::resolve_or_prompt_admin_password(&db).await?;
```

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --offline --lib onboarding::admin_bootstrap_tests
```

Expected: PASS:

```text
test onboarding::admin_bootstrap_tests::bootstrap_seeds_admin_user_when_empty ... ok
test onboarding::admin_bootstrap_tests::bootstrap_is_noop_when_admin_user_already_exists ... ok
test result: ok. 2 passed
```

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs src/main.rs
git commit -m "feat: admin_users bootstrap (TTY prompt / headless random-generate)"
```

---

## Phase B — Session lifecycle, login/logout, settings

**Parallelism:** Treat Phase A as fully landed first. B1 is a leaf. B2 must run solo relative to everything else that touches `AppState`/`main.rs` — sequence directly after A6, before B3. B4 lands in `src/auth/middleware.rs` after A4; can be drafted parallel with B5 if non-overlapping insertion points are agreed, otherwise sequence them (small file). B3 depends on B1, B2, A5, A6. B6 needs only B1's `delete_expired`. B7 depends only on A4 and can run fully parallel with B1–B6.

### Task B1: Session token issuance/validation module

**Files:**
- Create: `src/admin/auth/session.rs`
- Modify: `src/admin/auth/mod.rs`

**Interfaces:**
- Consumes: A1 `admin_sessions`, existing `sha2`, existing `rand`.
- Produces:
  ```rust
  pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
  pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

  #[derive(sqlx::FromRow, Clone, Debug)]
  pub struct SessionRow {
      pub token_hash: String,
      pub created_at: DateTime<Utc>,
      pub expires_at: DateTime<Utc>,
  }

  #[derive(Clone, Debug)]
  pub struct AdminSession {
      pub token_hash: String,
  }

  pub async fn create_session(db: &SqlitePool) -> Result<(String, DateTime<Utc>), AppError>;
  pub async fn validate_session(db: &SqlitePool, raw_token: &str) -> Result<Option<SessionRow>, AppError>;
  pub async fn renew_if_needed(db: &SqlitePool, token_hash: &str, created_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> Result<(), AppError>;
  pub async fn delete_all_sessions(db: &SqlitePool) -> Result<(), AppError>;
  pub async fn delete_all_sessions_except(db: &SqlitePool, keep_token_hash: &str) -> Result<(), AppError>;
  pub async fn delete_expired(db: &SqlitePool) -> Result<u64, AppError>;

  pub fn is_https(headers: &HeaderMap) -> bool;
  pub fn cookie_name(is_https: bool) -> &'static str;
  pub fn build_set_cookie(raw_token: &str, expires_at: DateTime<Utc>, is_https: bool) -> String;
  pub fn build_clear_cookie(is_https: bool) -> String;
  pub fn extract_cookie<'a>(headers: &'a HeaderMap, is_https: bool) -> Option<&'a str>;
  ```

- [ ] **Step 1: Write the failing test**

Create `src/admin/auth/session.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use axum::http::{HeaderMap, HeaderValue};
    use chrono::{Duration as ChronoDuration, Utc};
    use sha2::{Digest, Sha256};

    async fn db() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_tests.db");
        init_pool(path.to_str().unwrap()).await.unwrap()
    }

    fn hash_for_test(raw: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    #[tokio::test]
    async fn create_session_issues_lookupable_token() {
        let db = db().await;
        let (raw, expires_at) = create_session(&db).await.unwrap();

        assert_eq!(raw.len(), 64);
        assert!(expires_at > Utc::now());

        let row = validate_session(&db, &raw).await.unwrap().unwrap();
        assert_eq!(row.token_hash, hash_for_test(&raw));
    }

    #[tokio::test]
    async fn validate_session_rejects_unknown_token() {
        let db = db().await;
        let row = validate_session(&db, "not-a-real-token").await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn validate_session_rejects_expired_token() {
        let db = db().await;
        let raw = "expired-token";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(now - ChronoDuration::hours(2))
        .bind(now - ChronoDuration::hours(1))
        .execute(&db)
        .await
        .unwrap();

        let row = validate_session(&db, raw).await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn renew_if_needed_skips_write_when_over_half_ttl_remains() {
        let db = db().await;
        let (raw, _) = create_session(&db).await.unwrap();
        let before = validate_session(&db, &raw).await.unwrap().unwrap();

        renew_if_needed(&db, &before.token_hash, before.created_at, before.expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, &raw).await.unwrap().unwrap();
        assert_eq!(after.expires_at, before.expires_at);
    }

    #[tokio::test]
    async fn renew_if_needed_extends_when_under_half_ttl_remains() {
        let db = db().await;
        let raw = "needs-renewal";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();
        let created_at = now - ChronoDuration::hours(20);
        let expires_at = now + ChronoDuration::hours(1);

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(created_at)
        .bind(expires_at)
        .execute(&db)
        .await
        .unwrap();

        renew_if_needed(&db, &token_hash, created_at, expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, raw).await.unwrap().unwrap();
        assert!(after.expires_at > expires_at);
    }

    #[tokio::test]
    async fn renew_if_needed_never_exceeds_absolute_lifetime_cap() {
        let db = db().await;
        let raw = "near-cap";
        let token_hash = hash_for_test(raw);
        let now = Utc::now();
        let created_at = now - ChronoDuration::hours(165);
        let expires_at = now + ChronoDuration::minutes(1);
        let cap = created_at + ChronoDuration::from_std(ABSOLUTE_LIFETIME).unwrap();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(&token_hash)
        .bind(created_at)
        .bind(expires_at)
        .execute(&db)
        .await
        .unwrap();

        renew_if_needed(&db, &token_hash, created_at, expires_at)
            .await
            .unwrap();

        let after = validate_session(&db, raw).await.unwrap().unwrap();
        assert!(after.expires_at <= cap);
    }

    #[tokio::test]
    async fn delete_expired_removes_only_expired_rows() {
        let db = db().await;
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES ('expired', ?, ?), ('valid', ?, ?)",
        )
        .bind(now - ChronoDuration::hours(2))
        .bind(now - ChronoDuration::hours(1))
        .bind(now)
        .bind(now + ChronoDuration::hours(1))
        .execute(&db)
        .await
        .unwrap();

        let deleted = delete_expired(&db).await.unwrap();
        assert_eq!(deleted, 1);

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions ORDER BY token_hash")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(remaining, vec!["valid".to_string()]);
    }

    #[tokio::test]
    async fn delete_all_sessions_removes_everything() {
        let db = db().await;
        create_session(&db).await.unwrap();
        create_session(&db).await.unwrap();

        delete_all_sessions(&db).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn delete_all_sessions_except_keeps_only_named_token() {
        let db = db().await;
        let (keep_raw, _) = create_session(&db).await.unwrap();
        let (drop_raw, _) = create_session(&db).await.unwrap();
        let keep_hash = hash_for_test(&keep_raw);
        let drop_hash = hash_for_test(&drop_raw);

        delete_all_sessions_except(&db, &keep_hash).await.unwrap();

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(remaining, vec![keep_hash]);
        assert!(!remaining.contains(&drop_hash));
    }

    #[test]
    fn cookie_uses_host_prefix_and_secure_only_when_forwarded_proto_is_https() {
        let mut https_headers = HeaderMap::new();
        https_headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let http_headers = HeaderMap::new();
        let expires = Utc::now() + ChronoDuration::hours(1);

        assert!(is_https(&https_headers));
        assert!(!is_https(&http_headers));
        assert_eq!(cookie_name(true), "__Host-admin_session");
        assert_eq!(cookie_name(false), "admin_session");

        let secure = build_set_cookie("tok123", expires, true);
        assert!(secure.starts_with("__Host-admin_session=tok123"));
        assert!(secure.contains("; Secure"));

        let insecure = build_set_cookie("tok123", expires, false);
        assert!(insecure.starts_with("admin_session=tok123"));
        assert!(!insecure.contains("; Secure"));
    }

    #[test]
    fn extract_cookie_parses_named_cookie_out_of_multiple() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("foo=bar; admin_session=tok123; theme=dark"),
        );

        assert_eq!(extract_cookie(&headers, false), Some("tok123"));
        assert_eq!(extract_cookie(&headers, true), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --offline --lib admin::auth::session
```

Expected: FAIL — compiler reports unresolved items such as `create_session`, `validate_session`, `delete_expired`, and missing module export if `src/admin/auth/mod.rs` does not yet declare `pub mod session;`.

- [ ] **Step 3: Write minimal implementation**

Create `src/admin/auth/session.rs` above the tests:

```rust
use axum::http::HeaderMap;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::time::Duration;

use crate::core::error::AppError;

pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct SessionRow {
    pub token_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct AdminSession {
    pub token_hash: String,
}

fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub async fn create_session(db: &SqlitePool) -> Result<(String, DateTime<Utc>), AppError> {
    use rand::RngCore;

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let raw_token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let token_hash = hash_token(&raw_token);
    let now = Utc::now();
    let expires_at = now + ChronoDuration::from_std(SESSION_TTL).unwrap();

    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
         VALUES (?, ?, ?)",
    )
    .bind(&token_hash)
    .bind(now)
    .bind(expires_at)
    .execute(db)
    .await?;

    Ok((raw_token, expires_at))
}

pub async fn validate_session(
    db: &SqlitePool,
    raw_token: &str,
) -> Result<Option<SessionRow>, AppError> {
    let hash = hash_token(raw_token);
    let row = sqlx::query_as::<_, SessionRow>(
        "SELECT token_hash, created_at, expires_at
         FROM admin_sessions
         WHERE token_hash = ?",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;

    Ok(row.filter(|r| r.expires_at > Utc::now()))
}

pub async fn renew_if_needed(
    db: &SqlitePool,
    token_hash: &str,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<(), AppError> {
    let now = Utc::now();
    let remaining = expires_at - now;
    let full_window = expires_at - created_at;
    if remaining * 2 > full_window {
        return Ok(());
    }

    let absolute_cap = created_at + ChronoDuration::from_std(ABSOLUTE_LIFETIME).unwrap();
    let candidate = now + ChronoDuration::from_std(SESSION_TTL).unwrap();
    let new_expiry = candidate.min(absolute_cap);
    if new_expiry <= expires_at {
        return Ok(());
    }

    sqlx::query("UPDATE admin_sessions SET expires_at = ? WHERE token_hash = ?")
        .bind(new_expiry)
        .bind(token_hash)
        .execute(db)
        .await?;

    Ok(())
}

pub async fn delete_all_sessions(db: &SqlitePool) -> Result<(), AppError> {
    sqlx::query("DELETE FROM admin_sessions").execute(db).await?;
    Ok(())
}

pub async fn delete_all_sessions_except(
    db: &SqlitePool,
    keep_token_hash: &str,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM admin_sessions WHERE token_hash != ?")
        .bind(keep_token_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_expired(db: &SqlitePool) -> Result<u64, AppError> {
    let res = sqlx::query("DELETE FROM admin_sessions WHERE expires_at < ?")
        .bind(Utc::now())
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

pub fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub fn cookie_name(is_https: bool) -> &'static str {
    if is_https {
        "__Host-admin_session"
    } else {
        "admin_session"
    }
}

pub fn build_set_cookie(raw_token: &str, expires_at: DateTime<Utc>, is_https: bool) -> String {
    let name = cookie_name(is_https);
    let max_age = (expires_at - Utc::now()).num_seconds().max(0);
    let mut cookie =
        format!("{name}={raw_token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}");
    if is_https {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn build_clear_cookie(is_https: bool) -> String {
    let name = cookie_name(is_https);
    let mut cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if is_https {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn extract_cookie<'a>(headers: &'a HeaderMap, is_https: bool) -> Option<&'a str> {
    let name = cookie_name(is_https);
    let raw = headers.get("cookie")?.to_str().ok()?;
    raw.split("; ").find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k == name).then_some(v)
    })
}
```

Modify `src/admin/auth/mod.rs`:

```rust
pub mod password;
pub mod session;
```

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --offline --lib admin::auth::session
```

Expected: PASS:

```text
test admin::auth::session::tests::create_session_issues_lookupable_token ... ok
test admin::auth::session::tests::validate_session_rejects_unknown_token ... ok
test admin::auth::session::tests::validate_session_rejects_expired_token ... ok
test admin::auth::session::tests::renew_if_needed_skips_write_when_over_half_ttl_remains ... ok
test admin::auth::session::tests::renew_if_needed_extends_when_under_half_ttl_remains ... ok
test admin::auth::session::tests::renew_if_needed_never_exceeds_absolute_lifetime_cap ... ok
test admin::auth::session::tests::delete_expired_removes_only_expired_rows ... ok
test admin::auth::session::tests::delete_all_sessions_removes_everything ... ok
test admin::auth::session::tests::delete_all_sessions_except_keeps_only_named_token ... ok
test admin::auth::session::tests::cookie_uses_host_prefix_and_secure_only_when_forwarded_proto_is_https ... ok
test admin::auth::session::tests::extract_cookie_parses_named_cookie_out_of_multiple ... ok
test result: ok. 11 passed
```

- [ ] **Step 5: Commit**

```bash
git add src/admin/auth/session.rs src/admin/auth/mod.rs
git commit -m "feat: session token issuance/validation + cookie helpers"
```
### Task B2: Login rate limiter + `ConnectInfo` wiring

**Files:**
- Modify: `src/core/state.rs`
- Create: `src/admin/auth/rate_limit.rs`
- Modify: `src/admin/auth/mod.rs`
- Modify: `src/main.rs`
- Modify: `tests/common/mod.rs`
- Modify: `src/app.rs`
- Modify: `src/providers/refresh_task.rs`
- Modify: `src/providers/refresh_lock.rs`
- Modify: `tests/admin_pools.rs`
- Modify: `tests/health_stats.rs`

> **Assembly note (Opus review finding #2/#6):** the original drafts of A4/B2 both omitted 3 of the 7
> real `AppState { .. }` hand-built literal sites in this codebase. Verified directly via
> `grep -rln "AppState {" src tests`: `src/app.rs`, `src/main.rs`, `tests/common/mod.rs`,
> `tests/health_stats.rs`, `tests/admin_pools.rs`, `src/providers/refresh_lock.rs`,
> `src/providers/refresh_task.rs`. All 7 must gain `login_attempts` in the same commit or
> `cargo test --offline --lib` / `--test admin_pools` / `--test health_stats` fail to compile.

**Interfaces:**
- Consumes: existing `dashmap`.
- Produces:
  ```rust
  pub type LoginAttemptMap = Arc<DashMap<IpAddr, AttemptState>>;

  #[derive(Clone, Debug, Default)]
  pub struct AttemptState {
      pub failures: u32,
      pub locked_until: Option<Instant>,
  }

  pub const FAILURE_THRESHOLD: u32 = 5;
  pub const MAX_LOCKOUT: Duration = Duration::from_secs(5 * 60);

  pub fn cooldown_for(failures_over_threshold: u32) -> Duration;
  pub fn is_locked_out(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) -> bool;
  pub fn record_failure(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant);
  pub fn record_success(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr);
  ```

- [ ] **Step 1: Write the failing test**

Create `src/admin/auth/rate_limit.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn is_locked_out_false_before_threshold() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..4 {
            record_failure(&map, ip(1), now);
        }

        assert!(!is_locked_out(&map, ip(1), now));
        assert_eq!(map.get(&ip(1)).unwrap().failures, 4);
    }

    #[test]
    fn is_locked_out_true_once_threshold_exceeded() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }

        assert!(is_locked_out(&map, ip(1), now));
    }

    #[test]
    fn lockout_duration_escalates_and_caps_at_five_minutes() {
        assert_eq!(cooldown_for(1), Duration::from_secs(2));
        assert_eq!(cooldown_for(2), Duration::from_secs(4));
        assert_eq!(cooldown_for(3), Duration::from_secs(8));
        assert_eq!(cooldown_for(99), MAX_LOCKOUT);
    }

    #[test]
    fn record_success_resets_failures_and_clears_lockout() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }
        assert!(is_locked_out(&map, ip(1), now));

        record_success(&map, ip(1));

        let state = map.get(&ip(1)).unwrap();
        assert_eq!(state.failures, 0);
        assert!(state.locked_until.is_none());
    }

    #[test]
    fn different_ips_tracked_independently() {
        let map = DashMap::new();
        let now = Instant::now();

        for _ in 0..6 {
            record_failure(&map, ip(1), now);
        }

        assert!(is_locked_out(&map, ip(1), now));
        assert!(!is_locked_out(&map, ip(2), now));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --offline --lib admin::auth::rate_limit
cargo build --offline
```

Expected: FAIL — first command reports unresolved functions/types in `admin::auth::rate_limit`; after adding the `AppState` field but before all struct literals are updated, `cargo build --offline` reports missing field `login_attempts` in `AppState` initializers in `src/main.rs`, `tests/common/mod.rs`, `src/app.rs`, or `src/providers/refresh_task.rs`.

- [ ] **Step 3: Write minimal implementation**

Create `src/admin/auth/rate_limit.rs` above the tests:

```rust
use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type LoginAttemptMap = Arc<DashMap<IpAddr, AttemptState>>;

pub const FAILURE_THRESHOLD: u32 = 5;
pub const MAX_LOCKOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default)]
pub struct AttemptState {
    pub failures: u32,
    pub locked_until: Option<Instant>,
}

/// Mirrors proxy::backoff::cooldown_for: 2s * 2^(n-1), capped.
pub fn cooldown_for(failures_over_threshold: u32) -> Duration {
    let level = failures_over_threshold.max(1);
    let secs = 2u64.saturating_mul(2u64.saturating_pow((level - 1).min(15) as u32));
    Duration::from_secs(secs).min(MAX_LOCKOUT)
}

pub fn is_locked_out(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) -> bool {
    map.get(&ip)
        .map(|s| matches!(s.locked_until, Some(until) if now < until))
        .unwrap_or(false)
}

pub fn record_failure(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) {
    let mut entry = map.entry(ip).or_default();
    entry.failures += 1;
    // >= not >: lock starting at the 5th recorded failure so the 6th *attempt*
    // is the one that gets blocked (matches the spec's "after 5 failures" and
    // the test below - review fix for an off-by-one caught in the Opus pass).
    if entry.failures >= FAILURE_THRESHOLD {
        let cooldown = cooldown_for(entry.failures - FAILURE_THRESHOLD);
        entry.locked_until = Some(now + cooldown);
    }
}

pub fn record_success(map: &DashMap<IpAddr, AttemptState>, ip: IpAddr) {
    map.entry(ip).and_modify(|s| {
        s.failures = 0;
        s.locked_until = None;
    });
}
```

Modify `src/admin/auth/mod.rs`:

```rust
pub mod password;
pub mod rate_limit;
pub mod session;
```

Modify `src/core/state.rs`:

```rust
use crate::admin::auth::rate_limit::LoginAttemptMap;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    pub snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    pub runtime: RuntimeStateMap,
    pub log_tx: RequestLogSender,
    pub refresh_locks: RefreshLocks,
    pub shared_secret: Arc<ArcSwap<String>>,
    pub secret_origin: SecretOrigin,
    pub login_attempts: LoginAttemptMap,
}
```

In every `AppState { ... }` literal in `src/main.rs`, `tests/common/mod.rs`, `src/app.rs`,
`src/providers/refresh_task.rs`, `src/providers/refresh_lock.rs`, `tests/admin_pools.rs`, and
`tests/health_stats.rs` (all 7 real construction sites - see the assembly note above), add:

```rust
login_attempts: std::sync::Arc::new(dashmap::DashMap::new()),
```

Modify both axum serve call sites in `src/main.rs` and `tests/common/mod.rs`:

```rust
axum::serve(
    listener,
    router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
)
.await?;
```

If the existing call returns a non-`anyhow` error in a test helper, preserve the current error handling shape and only replace the served service expression.

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --offline --lib admin::auth::rate_limit
cargo test --offline --lib
```

Expected: PASS:

```text
test admin::auth::rate_limit::tests::is_locked_out_false_before_threshold ... ok
test admin::auth::rate_limit::tests::is_locked_out_true_once_threshold_exceeded ... ok
test admin::auth::rate_limit::tests::lockout_duration_escalates_and_caps_at_five_minutes ... ok
test admin::auth::rate_limit::tests::record_success_resets_failures_and_clears_lockout ... ok
test admin::auth::rate_limit::tests::different_ips_tracked_independently ... ok
test result: ok. 5 passed
```

`cargo test --offline --lib` also passes, proving all `AppState` literal sites compile with `login_attempts`.

- [ ] **Step 5: Commit**

```bash
git add src/core/state.rs src/admin/auth/rate_limit.rs src/admin/auth/mod.rs src/main.rs tests/common/mod.rs src/app.rs src/providers/refresh_task.rs src/providers/refresh_lock.rs tests/admin_pools.rs tests/health_stats.rs
git commit -m "feat: login rate limiter + ConnectInfo wiring for both axum::serve call sites"
```
### Task B3: Login / logout / password-change handlers

**Files:**
- Create: `src/admin/auth/routes.rs`
- Modify: `src/admin/mod.rs`
- Modify: `src/admin/auth/mod.rs`

**Interfaces:**
- Consumes: B1 `session`, B2 `rate_limit`, A5 `password`, A6 `admin_users`.
- Produces:
  ```rust
  pub fn public_routes() -> Router<AppState>;
  pub fn routes() -> Router<AppState>;
  ```

- [ ] **Step 1: Write the failing test**

Create `src/admin/auth/routes.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::auth::{password, session};
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::middleware;
    use dashmap::DashMap;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth_routes.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();
        let (log_tx, _log_rx) = tokio::sync::mpsc::channel(16);

        AppState {
            db,
            http: reqwest::Client::new(),
            config: Arc::new(Config {
                sqlite_path: path.to_string_lossy().to_string(),
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                shared_secret: "test-secret".to_string(),
                request_timeout_secs: 30,
            }),
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: Vec::new(),
                pools: Vec::new(),
            })),
            runtime: Arc::new(DashMap::new()),
            log_tx,
            refresh_locks: Arc::new(DashMap::new()),
            shared_secret: Arc::new(ArcSwap::from_pointee("test-secret".to_string())),
            secret_origin: SecretOrigin::SidecarFile,
            login_attempts: Arc::new(DashMap::new()),
        }
    }

    async fn seed_admin(db: &sqlx::SqlitePool, plain: &str) {
        let hash = password::hash_password(plain).unwrap();
        sqlx::query(
            "INSERT INTO admin_users (id, username, password_hash, updated_at)
             VALUES (1, 'admin', ?, '2026-01-01T00:00:00Z')",
        )
        .bind(hash)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn login_succeeds_with_correct_credentials_and_sets_cookie() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"username":"admin","password":"correct-password"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let set_cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("admin_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn login_rejects_wrong_password() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"username":"admin","password":"wrong"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_locks_out_after_five_failures() {
        let state = state().await;
        seed_admin(&state.db, "correct-password").await;

        let app = public_routes().with_state(state);

        let mut last = StatusCode::OK;
        for _ in 0..6 {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/admin/auth/login")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({"username":"admin","password":"wrong"}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            last = res.status();
        }

        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn logout_deletes_all_sessions_not_just_current() {
        let state = state().await;
        let (raw_a, _) = session::create_session(&state.db).await.unwrap();
        let (_raw_b, _) = session::create_session(&state.db).await.unwrap();

        let app = routes().with_state(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/logout")
                    .header(header::COOKIE, format!("admin_session={raw_a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn password_change_requires_current_password() {
        let state = state().await;
        seed_admin(&state.db, "old-password").await;

        let app = routes()
            .route_layer(middleware::from_fn(
                |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(session::AdminSession {
                        token_hash: "current".to_string(),
                    });
                    next.run(req).await
                },
            ))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/admin/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"current_password":"wrong","new_password":"new-password"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(password::verify_password(&stored, "old-password"));
    }

    #[tokio::test]
    async fn password_change_invalidates_other_sessions_but_keeps_current() {
        let state = state().await;
        seed_admin(&state.db, "old-password").await;
        let (raw_a, _) = session::create_session(&state.db).await.unwrap();
        let (raw_b, _) = session::create_session(&state.db).await.unwrap();
        let current = session::validate_session(&state.db, &raw_a)
            .await
            .unwrap()
            .unwrap();
        let other = session::validate_session(&state.db, &raw_b)
            .await
            .unwrap()
            .unwrap();

        let current_hash = current.token_hash.clone();
        let other_hash = other.token_hash.clone();

        let app = routes()
            .route_layer(middleware::from_fn(move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let current_hash = current_hash.clone();
                async move {
                    req.extensions_mut().insert(session::AdminSession {
                        token_hash: current_hash,
                    });
                    next.run(req).await
                }
            }))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/admin/auth/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"current_password":"old-password","new_password":"new-password"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let kept: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions WHERE token_hash = ?")
                .bind(&current.token_hash)
                .fetch_optional(&state.db)
                .await
                .unwrap();
        let removed: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM admin_sessions WHERE token_hash = ?")
                .bind(&other_hash)
                .fetch_optional(&state.db)
                .await
                .unwrap();

        assert_eq!(kept, Some(current.token_hash));
        assert!(removed.is_none());

        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM admin_users WHERE id = 1")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(password::verify_password(&stored, "new-password"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --offline --lib admin::auth::routes
```

Expected: FAIL — compiler reports unresolved functions `public_routes` and `routes`, and missing module export for `admin::auth::routes`.

- [ ] **Step 3: Write minimal implementation**

Create `src/admin/auth/routes.rs` above the tests:

```rust
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Instant;

use crate::admin::auth::{password, rate_limit, session};
use crate::core::error::AppError;
use crate::core::state::AppState;

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/admin/auth/login", post(login))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/auth/logout", post(logout))
        .route("/admin/auth/password", patch(change_password))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    let ip = addr.ip();
    let now = Instant::now();

    if rate_limit::is_locked_out(&state.login_attempts, ip, now) {
        tracing::warn!(%ip, "admin login blocked: rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"too many failed attempts, try again later"}})),
        )
            .into_response();
    }

    let row: Option<(String, String)> = match sqlx::query_as(
        "SELECT username, password_hash FROM admin_users WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(row) => row,
        Err(e) => return AppError::from(e).into_response(),
    };

    let ok = row
        .as_ref()
        .map(|(username, hash)| *username == req.username && password::verify_password(hash, &req.password))
        .unwrap_or(false);

    if !ok {
        rate_limit::record_failure(&state.login_attempts, ip, now);
        tracing::warn!(username = %req.username, %ip, "admin login failed");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"invalid username or password"}})),
        )
            .into_response();
    }

    rate_limit::record_success(&state.login_attempts, ip);

    let (raw_token, expires_at) = match session::create_session(&state.db).await {
        Ok(session) => session,
        Err(e) => return e.into_response(),
    };

    let https = session::is_https(&headers);
    let cookie = session::build_set_cookie(&raw_token, expires_at, https);

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"ok": true})),
    )
        .into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = session::delete_all_sessions(&state.db).await {
        return e.into_response();
    }

    let https = session::is_https(&headers);
    (
        StatusCode::OK,
        [(header::SET_COOKIE, session::build_clear_cookie(https))],
        Json(json!({"ok": true})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    Extension(sess): Extension<session::AdminSession>,
    Json(req): Json<PasswordChangeRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT username, password_hash FROM admin_users WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await?;

    let (_username, hash) =
        row.ok_or_else(|| AppError::Internal("admin_users row missing".into()))?;

    if !password::verify_password(&hash, &req.current_password) {
        return Err(AppError::Unauthorized);
    }

    if req.new_password.trim().is_empty() {
        return Err(AppError::BadRequest("new_password cannot be empty".into()));
    }

    let new_hash = password::hash_password(&req.new_password)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    sqlx::query("UPDATE admin_users SET password_hash = ?, updated_at = ? WHERE id = 1")
        .bind(&new_hash)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&state.db)
        .await?;

    session::delete_all_sessions_except(&state.db, &sess.token_hash).await?;

    Ok(Json(json!({"ok": true})))
}
```

Modify `src/admin/auth/mod.rs`:

```rust
pub mod password;
pub mod rate_limit;
pub mod routes;
pub mod session;
```

Ensure `src/admin/mod.rs` exposes the auth module after the A3 directory conversion:

```rust
pub mod auth;
```

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --offline --lib admin::auth::routes
```

Expected: PASS:

```text
test admin::auth::routes::tests::login_succeeds_with_correct_credentials_and_sets_cookie ... ok
test admin::auth::routes::tests::login_rejects_wrong_password ... ok
test admin::auth::routes::tests::login_locks_out_after_five_failures ... ok
test admin::auth::routes::tests::logout_deletes_all_sessions_not_just_current ... ok
test admin::auth::routes::tests::password_change_requires_current_password ... ok
test admin::auth::routes::tests::password_change_invalidates_other_sessions_but_keeps_current ... ok
test result: ok. 6 passed
```

- [ ] **Step 5: Commit**

```bash
git add src/admin/auth/routes.rs src/admin/auth/mod.rs src/admin/mod.rs
git commit -m "feat: login/logout/password-change handlers"
```
### Task B4: `require_admin_session` middleware with Bearer fallback

**Files:**
- Modify: `src/auth/middleware.rs`

**Interfaces:**
- Consumes: B1 `session::{validate_session, renew_if_needed, AdminSession, cookie helpers}`, A4 `state.shared_secret`.
- Produces:
  ```rust
  pub async fn require_admin_session(
      State(state): State<AppState>,
      mut req: Request,
      next: Next,
  ) -> Response
  ```

- [ ] **Step 1: Write the failing test**

Add to `src/auth/middleware.rs`:

```rust
#[cfg(test)]
mod require_admin_session_tests {
    use super::*;
    use crate::admin::auth::session;
    use crate::core::config::Config;
    use crate::core::db::init_pool;
    use crate::core::state::{AppState, ConfigSnapshot, SecretOrigin};
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use dashmap::DashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("require_admin_session.db");
        let db = init_pool(path.to_str().unwrap()).await.unwrap();
        let (log_tx, _log_rx) = tokio::sync::mpsc::channel(16);

        AppState {
            db,
            http: reqwest::Client::new(),
            config: Arc::new(Config {
                sqlite_path: path.to_string_lossy().to_string(),
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                shared_secret: "test-secret".to_string(),
                request_timeout_secs: 30,
            }),
            snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
                providers: Vec::new(),
                pools: Vec::new(),
            })),
            runtime: Arc::new(DashMap::new()),
            log_tx,
            refresh_locks: Arc::new(DashMap::new()),
            shared_secret: Arc::new(ArcSwap::from_pointee("test-secret".to_string())),
            secret_origin: SecretOrigin::SidecarFile,
            login_attempts: Arc::new(DashMap::new()),
        }
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .route("/protected", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_admin_session,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn require_admin_session_rejects_with_neither_cookie_nor_bearer() {
        let state = state().await;
        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_admin_session_accepts_valid_bearer() {
        let state = state().await;
        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_admin_session_accepts_valid_session_cookie() {
        let state = state().await;
        let (raw, _) = session::create_session(&state.db).await.unwrap();

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, format!("admin_session={raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_admin_session_rejects_expired_session_cookie() {
        let state = state().await;
        let raw = "expired";
        let token_hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(raw.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let now = chrono::Utc::now();

        sqlx::query(
            "INSERT INTO admin_sessions (token_hash, created_at, expires_at)
             VALUES (?, ?, ?)",
        )
        .bind(token_hash)
        .bind(now - chrono::Duration::hours(2))
        .bind(now - chrono::Duration::hours(1))
        .execute(&state.db)
        .await
        .unwrap();

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, "admin_session=expired")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_admin_session_falls_back_to_bearer_when_cookie_is_garbage() {
        let state = state().await;

        let res = app(state)
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::COOKIE, "admin_session=garbage")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --offline --lib auth::middleware::require_admin_session_tests
```

Expected: FAIL — compiler reports `cannot find value require_admin_session in this scope`.

- [ ] **Step 3: Write minimal implementation**

Modify `src/auth/middleware.rs`:

```rust
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::admin::auth::session;
use crate::core::state::AppState;

pub async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == state.shared_secret.load().as_str())
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "unauthorized" } })),
        )
            .into_response()
    }
}

pub async fn require_admin_session(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let https = session::is_https(headers);

    if let Some(raw) = session::extract_cookie(headers, https) {
        if let Ok(Some(row)) = session::validate_session(&state.db, raw).await {
            let _ = session::renew_if_needed(
                &state.db,
                &row.token_hash,
                row.created_at,
                row.expires_at,
            )
            .await;

            req.extensions_mut().insert(session::AdminSession {
                token_hash: row.token_hash,
            });
            return next.run(req).await;
        }
    }

    let bearer_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == state.shared_secret.load().as_str())
        .unwrap_or(false);

    if bearer_ok {
        return next.run(req).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "message": "unauthorized" } })),
    )
        .into_response()
}
```

- [ ] **Step 4: Run to verify it passes**

```bash
cargo test --offline --lib auth::middleware::require_admin_session_tests
cargo test --offline --lib auth::middleware
```

Expected: PASS:

```text
test auth::middleware::require_admin_session_tests::require_admin_session_rejects_with_neither_cookie_nor_bearer ... ok
test auth::middleware::require_admin_session_tests::require_admin_session_accepts_valid_bearer ... ok
test auth::middleware::require_admin_session_tests::require_admin_session_accepts_valid_session_cookie ... ok
test auth::middleware::require_admin_session_tests::require_admin_session_rejects_expired_session_cookie ... ok
test auth::middleware::require_admin_session_tests::require_admin_session_falls_back_to_bearer_when_cookie_is_garbage ... ok
test result: ok. 5 passed
```

The broader `auth::middleware` run also keeps all pre-existing `require_bearer` tests green.

- [ ] **Step 5: Commit**

```bash
git add src/auth/middleware.rs
git commit -m "feat: require_admin_session middleware (cookie-or-bearer fallback)"
```
### Task B5: CSRF header-check middleware

**Files:** Modify `src/auth/middleware.rs`

**Interfaces:** Consumes nothing new. Produces a middleware E1 layers across all of `/admin/*` (both strata, including login).

```rust
pub async fn require_csrf_header(req: Request, next: Next) -> Response {
    if req.method() != Method::GET {
        let ok = req.headers()
            .get("x-requested-with")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == "1router-ui")
            .unwrap_or(false);
        if !ok {
            return (StatusCode::FORBIDDEN,
                Json(json!({"error":{"message":"missing X-Requested-With header"}}))).into_response();
        }
    }
    next.run(req).await
}
```
Note: uses `axum::middleware::from_fn` (no `State`) since it needs no `AppState` — cheap, independently testable, no dependency on B1–B4.

Tests:
- `csrf_allows_get_without_header`
- `csrf_rejects_post_without_header` → 403
- `csrf_allows_post_with_correct_header_value`
- `csrf_rejects_post_with_wrong_header_value` (present but wrong string) → 403

- [ ] **Step 2:** `cargo test --offline --lib auth::middleware::tests::csrf` → FAIL.
- [ ] **Step 4:** PASS, 4 tests.
- [ ] **Step 5:** `git add src/auth/middleware.rs && git commit -m "feat: require_csrf_header middleware"`
### Task B6: Session cleanup background sweep

**Files:**
- Create: `src/admin/auth/cleanup.rs`
- Modify: `src/admin/auth/mod.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: B1 `session::delete_expired`.
- Produces:
  ```rust
  pub fn spawn_session_cleanup(state: AppState)
  ```
  plus a boot-time expired-session sweep in `main.rs`.

- [ ] **Step 1: Write the failing test**

No new `#[test]` for the interval loop; this mirrors `providers::refresh_task::spawn_background_refresh`, where the long-running tokio interval wrapper is smoke-verified by compilation and the DB behavior is unit-tested at the query layer. The failing check is compilation against the new module symbol by adding the call site first.

Modify `src/main.rs` at the intended boot-time location:

```rust
if let Err(e) = router::admin::auth::session::delete_expired(&db).await {
    tracing::warn!(error = %e, "boot-time admin session sweep failed");
}
```

And after the existing background refresh spawn:

```rust
router::admin::auth::cleanup::spawn_session_cleanup(state.clone());
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo build --offline
```

Expected: FAIL — compiler reports `could not find cleanup in auth`.

- [ ] **Step 3: Write minimal implementation**

Create `src/admin/auth/cleanup.rs`:

```rust
use std::time::Duration;

use crate::admin::auth::session;
use crate::core::state::AppState;

const CLEANUP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Structural mirror of providers::refresh_task::spawn_background_refresh.
pub fn spawn_session_cleanup(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);

        loop {
            interval.tick().await;
            match session::delete_expired(&state.db).await {
                Ok(deleted) if deleted > 0 => tracing::info!(
                    deleted,
                    "admin session cleanup swept expired rows"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    error = %e,
                    "admin session cleanup sweep failed"
                ),
            }
        }
    });
}
```

Modify `src/admin/auth/mod.rs`:

```rust
pub mod cleanup;
pub mod password;
pub mod rate_limit;
pub mod routes;
pub mod session;
```

Modify `src/main.rs` before `AppState` construction or before spawning background tasks:

```rust
if let Err(e) = router::admin::auth::session::delete_expired(&db).await {
    tracing::warn!(error = %e, "boot-time admin session sweep failed");
}
```

Modify `src/main.rs` alongside the existing provider refresh background task:

```rust
spawn_background_refresh(state.clone());
router::admin::auth::cleanup::spawn_session_cleanup(state.clone());
```

If `main.rs` imports crate modules at the top instead of using fully qualified `router::...` paths, use the existing import style and keep the two inserted calls equivalent.

- [ ] **Step 4: Run to verify it passes**

```bash
cargo build --offline
cargo test --offline --lib admin::auth::session::tests::delete_expired_removes_only_expired_rows
```

Expected: PASS:

```text
Finished dev [unoptimized] target(s)
test admin::auth::session::tests::delete_expired_removes_only_expired_rows ... ok
test result: ok. 1 passed
```

Manual smoke verification, matching the refresh-task precedent: temporarily lower `CLEANUP_INTERVAL` in a scratch worktree, insert an expired `admin_sessions` row, run the server, and confirm the log line `admin session cleanup swept expired rows` appears and the row is deleted. Do not commit the temporary interval change.

- [ ] **Step 5: Commit**

```bash
git add src/admin/auth/cleanup.rs src/admin/auth/mod.rs src/main.rs
git commit -m "feat: session cleanup background sweep + boot-time sweep"
```
### Task B7: `GET/PATCH /admin/settings/shared-secret`

> **Assembly note:** this is the canonical B7 (Opus review finding #3 flagged two independently-drafted, incompatible versions of this task - this one's `masked`/`origin`/`?reveal=true` contract was judged correct and is kept; a thinner competing draft was discarded). It has also been relocated from flat `src/admin.rs` into `src/admin/settings.rs`, since Task A3 (module directory conversion) lands first in this assembled plan and every other Phase B task already assumes the `src/admin/` directory shape.

**Corrections grounded in current code:**
- Current `providers::routes::mask` is private and provider-shaped; B7 should add a small local secret-mask helper in `src/admin/settings.rs` instead of trying to reuse the provider helper.
- `AppError::Conflict` already exists and maps to HTTP 409, so B7 only needs to return it for `SecretOrigin::Env`.
- The route must use axum 0.7 syntax conventions from `CLAUDE.md`; this route has no dynamic segment, so no `:id`/`{id}` issue applies.

**Files:**
- Create: `src/admin/settings.rs`
- Modify: `src/admin/mod.rs`
- Create: `tests/admin_settings.rs`

**Interfaces:**
- Consumes:
  - `crate::core::state::{AppState, SecretOrigin}`
  - `crate::core::config::persist_secret(sqlite_path: &str, secret: &str) -> anyhow::Result<()>`
  - `AppError::Conflict(String)`
  - `AppError::BadRequest(String)`
  - A4's live `AppState.shared_secret: Arc<ArcSwap<String>>`
- Produces:
  ```rust
  // Route wiring:
  // GET   /admin/settings/shared-secret
  // GET   /admin/settings/shared-secret?reveal=true
  // PATCH /admin/settings/shared-secret

  #[derive(serde::Deserialize)]
  struct SharedSecretQuery {
      #[serde(default)]
      reveal: bool,
  }

  #[derive(serde::Deserialize)]
  struct SharedSecretPatch {
      shared_secret: String,
  }

  #[derive(serde::Serialize)]
  struct SharedSecretResponse {
      shared_secret: String,
      masked: bool,
      origin: crate::core::state::SecretOrigin,
  }
  ```

- [ ] **Step 1: Write the failing tests**

Create `tests/admin_settings.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use router::app::build_router;
use router::core::config::{secret_file_path, Config};
use router::core::db::init_pool;
use router::core::http_client::build_client;
use router::core::state::{AppState, ConfigSnapshot, SecretOrigin};
use serde_json::json;
use tower::ServiceExt;

async fn test_state(secret_origin: SecretOrigin) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = init_pool(db_path.to_str().unwrap()).await.unwrap();
    let cfg = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        sqlite_path: db_path.to_string_lossy().into_owned(),
        shared_secret: "initial".into(),
        seed_path: None,
        connect_timeout: Duration::from_secs(1),
        ttfb_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_body_bytes: 1024,
        drain_timeout: Duration::from_secs(1),
    };
    let (log_tx, _log_rx) = tokio::sync::mpsc::channel(8);
    let state = AppState {
        db,
        http: build_client(&cfg),
        config: Arc::new(cfg.clone()),
        shared_secret: Arc::new(ArcSwap::from_pointee(cfg.shared_secret.clone())),
        secret_origin,
        snapshot: Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
            providers: vec![],
            pools: vec![],
        })),
        runtime: Arc::new(dashmap::DashMap::new()),
        log_tx,
        refresh_locks: Arc::new(dashmap::DashMap::new()),
    };
    (state, dir)
}

fn request(method: Method, uri: &str, secret: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"));

    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };

    builder.body(body).unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn shared_secret_get_masks_by_default_and_reveals_explicitly() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let router = build_router(state);

    let masked = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(masked.status(), StatusCode::OK);
    let body = json_body(masked).await;
    assert_eq!(body["shared_secret"], "***tial");
    assert_eq!(body["masked"], true);
    assert_eq!(body["origin"], "sidecar_file");

    let revealed = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(revealed.status(), StatusCode::OK);
    let body = json_body(revealed).await;
    assert_eq!(body["shared_secret"], "initial");
    assert_eq!(body["masked"], false);
    assert_eq!(body["origin"], "sidecar_file");
}

#[tokio::test]
async fn shared_secret_patch_persists_and_rotates_live_bearer_secret() {
    let (state, _dir) = test_state(SecretOrigin::SidecarFile).await;
    let secret_path = secret_file_path(&state.config.sqlite_path);
    let router = build_router(state);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/shared-secret",
            "initial",
            Some(json!({ "shared_secret": "rotated-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let body = json_body(patched).await;
    assert_eq!(body["shared_secret"], "***cret");
    assert_eq!(body["masked"], true);
    assert_eq!(body["origin"], "sidecar_file");
    assert_eq!(
        std::fs::read_to_string(secret_path).unwrap(),
        "rotated-secret"
    );

    let old_secret = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(old_secret.status(), StatusCode::UNAUTHORIZED);

    let new_secret = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "rotated-secret",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(new_secret.status(), StatusCode::OK);
    let body = json_body(new_secret).await;
    assert_eq!(body["shared_secret"], "rotated-secret");
}

#[tokio::test]
async fn shared_secret_patch_conflicts_when_secret_origin_is_env() {
    let (state, _dir) = test_state(SecretOrigin::Env).await;
    let secret_path = secret_file_path(&state.config.sqlite_path);
    let router = build_router(state);

    let patched = router
        .clone()
        .oneshot(request(
            Method::PATCH,
            "/admin/settings/shared-secret",
            "initial",
            Some(json!({ "shared_secret": "rotated-secret" })),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::CONFLICT);
    let body = json_body(patched).await;
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("ROUTER_SHARED_SECRET"));
    assert!(!secret_path.exists());

    let old_secret_still_works = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "initial",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(old_secret_still_works.status(), StatusCode::OK);

    let new_secret_does_not_work = router
        .oneshot(request(
            Method::GET,
            "/admin/settings/shared-secret?reveal=true",
            "rotated-secret",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(new_secret_does_not_work.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: Run to verify it fails**

Run:

```bash
cargo test --offline --test admin_settings
```

Expected: FAIL because `/admin/settings/shared-secret` is not routed yet:

```text
thread 'shared_secret_get_masks_by_default_and_reveals_explicitly' panicked at tests/admin_settings.rs:82:5:
assertion `left == right` failed
  left: 404
 right: 200
```

- [ ] **Step 3: Write minimal implementation**

Add to `src/admin/mod.rs`:

```rust
pub mod settings;
```

Create `src/admin/settings.rs`:

```rust
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
```

Also import config persistence and `SecretOrigin`:

```rust
use crate::core::config;
use crate::core::state::{reload_snapshot, AppState, SecretOrigin};
```

Wire the new route in `routes()`:

```rust
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/admin/settings/shared-secret",
        get(get_shared_secret).patch(patch_shared_secret),
    )
}
```

Add the request/response types and handlers near the existing route handlers:

```rust
#[derive(Debug, Deserialize)]
struct SharedSecretQuery {
    #[serde(default)]
    reveal: bool,
}

#[derive(Debug, Deserialize)]
struct SharedSecretPatch {
    shared_secret: String,
}

#[derive(Debug, Serialize)]
struct SharedSecretResponse {
    shared_secret: String,
    masked: bool,
    origin: SecretOrigin,
}

fn mask_secret(secret: &str) -> String {
    let tail = secret
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();

    if tail.is_empty() {
        "***".to_string()
    } else {
        format!("***{tail}")
    }
}

fn shared_secret_response(secret: &str, reveal: bool, origin: SecretOrigin) -> SharedSecretResponse {
    SharedSecretResponse {
        shared_secret: if reveal {
            secret.to_string()
        } else {
            mask_secret(secret)
        },
        masked: !reveal,
        origin,
    }
}

async fn get_shared_secret(
    State(s): State<AppState>,
    Query(q): Query<SharedSecretQuery>,
) -> Result<Json<SharedSecretResponse>, AppError> {
    let secret = s.shared_secret.load();
    Ok(Json(shared_secret_response(
        secret.as_str(),
        q.reveal,
        s.secret_origin,
    )))
}

async fn patch_shared_secret(
    State(s): State<AppState>,
    Json(body): Json<SharedSecretPatch>,
) -> Result<Json<SharedSecretResponse>, AppError> {
    if matches!(s.secret_origin, SecretOrigin::Env) {
        return Err(AppError::Conflict(
            "ROUTER_SHARED_SECRET is set; change or unset the environment variable instead"
                .to_string(),
        ));
    }

    let new_secret = body.shared_secret.trim().to_string();
    if new_secret.is_empty() {
        return Err(AppError::BadRequest(
            "shared_secret must not be empty".to_string(),
        ));
    }

    config::persist_secret(&s.config.sqlite_path, &new_secret)
        .map_err(|e| AppError::Internal(e.to_string()))?;
    s.shared_secret.store(Arc::new(new_secret.clone()));

    Ok(Json(shared_secret_response(
        &new_secret,
        false,
        s.secret_origin,
    )))
}
```

- [ ] **Step 4: Run to verify it passes**

Run:

```bash
cargo test --offline --test admin_settings
```

Expected: PASS:

```text
running 3 tests
test shared_secret_get_masks_by_default_and_reveals_explicitly ... ok
test shared_secret_patch_conflicts_when_secret_origin_is_env ... ok
test shared_secret_patch_persists_and_rotates_live_bearer_secret ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Also run the admin module unit tests to confirm the A3 move + this addition compose cleanly:

```bash
cargo test --offline --lib admin
```

Expected: PASS, including the pre-existing (relocated, unchanged) import regression test:

```text
test admin::tests::import_is_all_or_nothing_on_failure ... ok
```

- [ ] **Step 5: Commit**

```bash
git add src/admin/settings.rs src/admin/mod.rs tests/admin_settings.rs
git commit -m "feat: add editable shared secret setting

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Phase C — Frontend

**Parallelism:** C1 must land first (scaffold), then C2 (apiClient). After that, C3, C4/C4b, C5, and C6 are independent files and can run in parallel worktrees. C7 (test tooling) lands incrementally alongside each page but is listed last since it touches `package.json`/`tsconfig.json` shared by all of them — treat its `package.json`/`tsconfig.json` edits as the final merge point for Phase C, same shape as `Cargo.toml` in Phase A.

### Task C1: Vite + React + TypeScript scaffold

**Files:**
- Create: `frontend/package.json`
- Create: `frontend/tsconfig.json`
- Create: `frontend/vite.config.ts`
- Create: `frontend/index.html`
- Create: `frontend/src/main.tsx`
- Create: `frontend/src/App.tsx`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: nothing.
- Produces: `npm run build` emitting `frontend/dist/` (the exact contract `build.rs` shells out to).
- Produces: `frontend/vite.config.ts` with `base: "/ui/"` so built asset URLs match the `/ui/*path` mount point, and `build.outDir: "dist"`.
- Produces: `frontend/src/App.tsx` with `react-router-dom` client routes for `/ui/login`, `/ui/providers`, `/ui/pools`, and `/ui/settings`.
- Produces: `frontend/package.json` key fields: name `"1router-admin-ui"`, private `true`, scripts `{ "build": "tsc -b && vite build", "dev": "vite", "test": "vitest run" }`, dependencies `{ "react": "^18", "react-dom": "^18", "react-router-dom": "^6" }`, devDependencies `{ "vite": "^5", "typescript": "^5", "@vitejs/plugin-react": "^4" }`.

- [ ] **Step 1: Write the failing test**

There is no component test before the scaffold exists; the TDD check is `npm run build` from `frontend/`. Create `frontend/package.json` first:

```json
{
  "name": "1router-admin-ui",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "scripts": {
    "build": "tsc -b && vite build",
    "dev": "vite",
    "test": "vitest run"
  },
  "dependencies": {
    "react": "^18.3.1",
    "react-dom": "^18.3.1",
    "react-router-dom": "^6.26.2"
  },
  "devDependencies": {
    "@vitejs/plugin-react": "^4.3.1",
    "typescript": "^5.6.2",
    "vite": "^5.4.8"
  }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npm run build`

Expected: FAIL — `tsc -b` cannot find `tsconfig.json`, or Vite cannot find `index.html`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "useDefineForClassFields": true,
    "lib": ["DOM", "DOM.Iterable", "ES2020"],
    "allowJs": false,
    "skipLibCheck": true,
    "esModuleInterop": true,
    "allowSyntheticDefaultImports": true,
    "strict": true,
    "forceConsistentCasingInFileNames": true,
    "module": "ESNext",
    "moduleResolution": "Node",
    "resolveJsonModule": true,
    "isolatedModules": true,
    "noEmit": true,
    "jsx": "react-jsx"
  },
  "include": ["src", "vite.config.ts"]
}
```

Create `frontend/vite.config.ts`:

```ts
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "/ui/",
  plugins: [react()],
  build: {
    outDir: "dist"
  }
});
```

Create `frontend/index.html`:

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>1router Admin</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
```

Create `frontend/src/main.tsx`:

```tsx
import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { App } from "./App";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <BrowserRouter>
      <App />
    </BrowserRouter>
  </React.StrictMode>
);
```

Create `frontend/src/App.tsx`:

```tsx
import { Navigate, NavLink, Route, Routes } from "react-router-dom";

function Placeholder({ title }: { title: string }) {
  return <h1>{title}</h1>;
}

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Placeholder title="Login" />} />
        <Route path="/ui/providers" element={<Placeholder title="Providers" />} />
        <Route path="/ui/pools" element={<Placeholder title="Pools" />} />
        <Route path="/ui/settings" element={<Placeholder title="Settings" />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
```

Modify `.gitignore`:

```gitignore
/target
*.db
*.db-wal
*.db-shm
frontend/node_modules/
frontend/dist/
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npm run build`

Expected: PASS — `tsc -b && vite build` completes and emits `frontend/dist/index.html`.

- [ ] **Step 5: Commit**

```bash
git add frontend/package.json frontend/tsconfig.json frontend/vite.config.ts frontend/index.html frontend/src/main.tsx frontend/src/App.tsx .gitignore
git commit -m "feat(ui): scaffold React admin app"
```

### Task C2: apiClient.ts

**Files:**
- Create: `frontend/src/lib/apiClient.ts`
- Create: `frontend/src/lib/apiClient.test.ts`

**Interfaces:**
- Consumes: C1 tooling.
- Produces: fetch wrapper C3-C6 all import.
- Produces: `X-Requested-With: 1router-ui` on every non-GET request, matching the backend CSRF middleware exactly.
- Produces: `credentials: "include"` on every fetch.
- Produces: 401 handling that redirects with `window.location.assign("/ui/login")` unless already on `/ui/login`.
- Produces: `apiJson<T>(path, init)` that throws with the server's `error.message` on non-ok responses.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/lib/apiClient.test.ts`:

```ts
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { apiFetch, apiJson } from "./apiClient";

describe("apiClient", () => {
  const originalLocation = window.location;

  beforeEach(() => {
    vi.restoreAllMocks();
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response(JSON.stringify({ ok: true }), { status: 200 }))
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: originalLocation
    });
  });

  it("apiFetch_sets_csrf_header_on_post_but_not_get", async () => {
    await apiFetch("/admin/providers");
    await apiFetch("/admin/providers", { method: "POST" });

    const fetchMock = vi.mocked(fetch);
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      credentials: "include"
    });
    expect(new Headers(fetchMock.mock.calls[0][1]?.headers).get("X-Requested-With")).toBeNull();
    expect(new Headers(fetchMock.mock.calls[1][1]?.headers).get("X-Requested-With")).toBe("1router-ui");
  });

  it("apiFetch_redirects_to_login_on_401_unless_already_on_login_page", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 401 })));
    const assign = vi.fn();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { pathname: "/ui/providers", assign }
    });

    await apiFetch("/admin/providers");
    expect(assign).toHaveBeenCalledWith("/ui/login");

    assign.mockClear();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { pathname: "/ui/login", assign }
    });

    await apiFetch("/admin/providers");
    expect(assign).not.toHaveBeenCalled();
  });

  it("apiJson_throws_with_server_error_message_on_non_ok", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ error: { message: "bad password" } }), {
          status: 401,
          headers: { "Content-Type": "application/json" }
        })
      )
    );

    await expect(apiJson("/admin/auth/login", { method: "POST" })).rejects.toThrow("bad password");
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/lib/apiClient.test.ts`

Expected: FAIL — `Cannot find module './apiClient'`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/src/lib/apiClient.ts`:

```ts
const CSRF_HEADER = "X-Requested-With";
const CSRF_VALUE = "1router-ui";

type ApiErrorBody = {
  error?: {
    message?: string;
  };
};

export async function apiFetch(path: string, init: RequestInit = {}) {
  const method = (init.method ?? "GET").toUpperCase();
  const headers = new Headers(init.headers);

  if (method !== "GET") {
    headers.set(CSRF_HEADER, CSRF_VALUE);
  }

  const response = await fetch(path, {
    ...init,
    method,
    headers,
    credentials: "include"
  });

  if (response.status === 401 && window.location.pathname !== "/ui/login") {
    window.location.assign("/ui/login");
  }

  return response;
}

export async function apiJson<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await apiFetch(path, init);
  const text = await response.text();
  const body = text ? (JSON.parse(text) as ApiErrorBody | T) : undefined;

  if (!response.ok) {
    const message =
      (body as ApiErrorBody | undefined)?.error?.message ?? `Request failed with status ${response.status}`;
    throw new Error(message);
  }

  return body as T;
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/lib/apiClient.test.ts`

Expected: PASS — 3 tests pass, including `apiFetch_sets_csrf_header_on_post_but_not_get`, `apiFetch_redirects_to_login_on_401_unless_already_on_login_page`, and `apiJson_throws_with_server_error_message_on_non_ok`.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/lib/apiClient.ts frontend/src/lib/apiClient.test.ts
git commit -m "feat(ui): add admin API client"
```

### Task C3: Login page

**Files:**
- Create: `frontend/src/pages/Login.tsx`
- Create: `frontend/src/pages/Login.test.tsx`
- Modify: `frontend/src/App.tsx`

**Interfaces:**
- Consumes: C1/C2.
- Produces: login page that posts `{ username, password }` to `/admin/auth/login`.
- Produces: successful login navigation to `/ui/providers`.
- Produces: rendering of the server's `error.message` on 401/429 without distinguishing which.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/pages/Login.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { Login } from "./Login";

function renderLogin() {
  render(
    <MemoryRouter initialEntries={["/ui/login"]}>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<h1>Providers</h1>} />
      </Routes>
    </MemoryRouter>
  );
}

describe("Login", () => {
  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn());
  });

  it("validates_required_fields_before_submit", async () => {
    renderLogin();
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(await screen.findByText("Username and password are required.")).toBeInTheDocument();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("posts_credentials_and_navigates_to_providers", async () => {
    vi.mocked(fetch).mockResolvedValue(new Response("{}", { status: 200 }));
    renderLogin();

    await userEvent.type(screen.getByLabelText("Username"), "admin");
    await userEvent.type(screen.getByLabelText("Password"), "secret");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/auth/login",
      expect.objectContaining({
        method: "POST",
        credentials: "include",
        body: JSON.stringify({ username: "admin", password: "secret" })
      })
    );
    expect(await screen.findByRole("heading", { name: "Providers" })).toBeInTheDocument();
  });

  it("renders_server_error_message", async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(JSON.stringify({ error: { message: "too many attempts" } }), { status: 429 })
    );
    renderLogin();

    await userEvent.type(screen.getByLabelText("Username"), "admin");
    await userEvent.type(screen.getByLabelText("Password"), "bad");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("too many attempts");
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/pages/Login.test.tsx`

Expected: FAIL — `Cannot find module './Login'`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/src/pages/Login.tsx`:

```tsx
import { FormEvent, useState } from "react";
import { useNavigate } from "react-router-dom";
import { apiJson } from "../lib/apiClient";

export function Login() {
  const navigate = useNavigate();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(null);

    if (!username.trim() || !password) {
      setError("Username and password are required.");
      return;
    }

    setSubmitting(true);
    try {
      await apiJson("/admin/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password })
      });
      navigate("/ui/providers");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Login failed.");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <section aria-labelledby="login-title">
      <h1 id="login-title">Login</h1>
      <form onSubmit={onSubmit}>
        <label>
          Username
          <input value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" />
        </label>
        <label>
          Password
          <input
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            type="password"
            autoComplete="current-password"
          />
        </label>
        {error ? <p role="alert">{error}</p> : null}
        <button type="submit" disabled={submitting}>
          Sign in
        </button>
      </form>
    </section>
  );
}
```

Modify `frontend/src/App.tsx`:

```tsx
import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";

function Placeholder({ title }: { title: string }) {
  return <h1>{title}</h1>;
}

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<Placeholder title="Providers" />} />
        <Route path="/ui/pools" element={<Placeholder title="Pools" />} />
        <Route path="/ui/settings" element={<Placeholder title="Settings" />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/pages/Login.test.tsx`

Expected: PASS — validation, navigation, and server error rendering tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/pages/Login.tsx frontend/src/pages/Login.test.tsx frontend/src/App.tsx
git commit -m "feat(ui): add admin login page"
```

### Task C4: Providers page

**Files:**
- Create: `frontend/src/pages/Providers.tsx`
- Create: `frontend/src/pages/Providers.form.test.tsx`
- Modify: `frontend/src/App.tsx`

**Interfaces:**
- Consumes: C1/C2 and C4b when `kind === "oauth_codex"`.
- Produces: list/create/edit/delete via existing `/admin/providers*`.
- Produces: state badge polled every ~5s from `GET /admin/providers/:id/state`.
- Produces: provider edit modal that hosts `CodexOAuthPanel` when `kind === "oauth_codex"`.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/pages/Providers.form.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Providers } from "./Providers";

const providers = [
  {
    id: "prov_1",
    name: "openai",
    wire_format: "openai",
    kind: "passthrough",
    base_url: "https://api.openai.com/v1",
    api_key: "sk-***",
    upstream_model: "gpt-4.1"
  }
];

describe("Providers", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify(providers), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/state") {
          return new Response(JSON.stringify({ state: "ready" }), { status: 200 });
        }
        if (url === "/admin/providers" && init?.method === "POST") {
          return new Response(JSON.stringify({ ...providers[0], id: "prov_2", name: "anthropic" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "PUT") {
          return new Response(JSON.stringify({ ...providers[0], upstream_model: "gpt-4.1-mini" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("lists_providers_and_polls_state_badges", async () => {
    render(<Providers />);

    expect(await screen.findByText("openai")).toBeInTheDocument();
    expect(await screen.findByText("ready")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1/state", expect.objectContaining({ credentials: "include" }));
  });

  it("creates_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.type(screen.getByLabelText("Name"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("Wire format"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("Kind"), "passthrough");
    await userEvent.type(screen.getByLabelText("Base URL"), "https://api.anthropic.com");
    await userEvent.type(screen.getByLabelText("API key"), "secret");
    await userEvent.type(screen.getByLabelText("Upstream model"), "claude-sonnet-4");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/providers",
        expect.objectContaining({
          method: "POST",
          body: expect.stringContaining("\"name\":\"anthropic\"")
        })
      )
    );
  });

  it("edits_and_deletes_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "Edit openai" }));
    await userEvent.clear(screen.getByLabelText("Upstream model"));
    await userEvent.type(screen.getByLabelText("Upstream model"), "gpt-4.1-mini");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));
    await waitFor(() => expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1", expect.objectContaining({ method: "PUT" })));

    await userEvent.click(screen.getByRole("button", { name: "Delete openai" }));
    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1", expect.objectContaining({ method: "DELETE" }));
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/pages/Providers.form.test.tsx`

Expected: FAIL — `Cannot find module './Providers'`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/src/pages/Providers.tsx`:

```tsx
import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { CodexOAuthPanel } from "../components/CodexOAuthPanel";

type Provider = {
  id: string;
  name: string;
  wire_format: string;
  kind: string;
  base_url?: string;
  api_key?: string;
  upstream_model: string;
};

type ProviderForm = Omit<Provider, "id">;

const emptyForm: ProviderForm = {
  name: "",
  wire_format: "openai",
  kind: "passthrough",
  base_url: "",
  api_key: "",
  upstream_model: ""
};

export function Providers() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [states, setStates] = useState<Record<string, string>>({});
  const [editing, setEditing] = useState<Provider | null>(null);
  const [form, setForm] = useState<ProviderForm>(emptyForm);
  const [modalOpen, setModalOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function loadProviders() {
    setProviders(await apiJson<Provider[]>("/admin/providers"));
  }

  useEffect(() => {
    void loadProviders();
  }, []);

  useEffect(() => {
    if (providers.length === 0) {
      return;
    }

    let cancelled = false;
    async function loadStates() {
      const entries = await Promise.all(
        providers.map(async (provider) => {
          const body = await apiJson<{ state: string }>(`/admin/providers/${provider.id}/state`);
          return [provider.id, body.state] as const;
        })
      );
      if (!cancelled) {
        setStates(Object.fromEntries(entries));
      }
    }

    void loadStates();
    const timer = window.setInterval(loadStates, 5000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [providers]);

  function openNew() {
    setEditing(null);
    setForm(emptyForm);
    setModalOpen(true);
  }

  function openEdit(provider: Provider) {
    setEditing(provider);
    setForm({
      name: provider.name,
      wire_format: provider.wire_format,
      kind: provider.kind,
      base_url: provider.base_url ?? "",
      api_key: "",
      upstream_model: provider.upstream_model
    });
    setModalOpen(true);
  }

  async function saveProvider(event: FormEvent) {
    event.preventDefault();
    setError(null);
    try {
      await apiJson(editing ? `/admin/providers/${editing.id}` : "/admin/providers", {
        method: editing ? "PUT" : "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(form)
      });
      setModalOpen(false);
      await loadProviders();
    } catch (error) {
      setError(error instanceof Error ? error.message : "Provider save failed.");
    }
  }

  async function deleteProvider(provider: Provider) {
    await apiJson(`/admin/providers/${provider.id}`, { method: "DELETE" });
    setProviders((current) => current.filter((item) => item.id !== provider.id));
  }

  return (
    <section aria-labelledby="providers-title">
      <h1 id="providers-title">Providers</h1>
      <button type="button" onClick={openNew}>
        New provider
      </button>
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Wire format</th>
            <th>Kind</th>
            <th>Model</th>
            <th>State</th>
            <th>Actions</th>
          </tr>
        </thead>
        <tbody>
          {providers.map((provider) => (
            <tr key={provider.id}>
              <td>{provider.name}</td>
              <td>{provider.wire_format}</td>
              <td>{provider.kind}</td>
              <td>{provider.upstream_model}</td>
              <td>{states[provider.id] ?? "checking"}</td>
              <td>
                <button type="button" onClick={() => openEdit(provider)} aria-label={`Edit ${provider.name}`}>
                  Edit
                </button>
                <button type="button" onClick={() => deleteProvider(provider)} aria-label={`Delete ${provider.name}`}>
                  Delete
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {modalOpen ? (
        <form aria-label="Provider form" onSubmit={saveProvider}>
          <label>
            Name
            <input value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} />
          </label>
          <label>
            Wire format
            <select value={form.wire_format} onChange={(event) => setForm({ ...form, wire_format: event.target.value })}>
              <option value="openai">openai</option>
              <option value="anthropic">anthropic</option>
            </select>
          </label>
          <label>
            Kind
            <select value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value })}>
              <option value="passthrough">passthrough</option>
              <option value="oauth_codex">oauth_codex</option>
            </select>
          </label>
          <label>
            Base URL
            <input value={form.base_url} onChange={(event) => setForm({ ...form, base_url: event.target.value })} />
          </label>
          <label>
            API key
            <input value={form.api_key} onChange={(event) => setForm({ ...form, api_key: event.target.value })} />
          </label>
          <label>
            Upstream model
            <input value={form.upstream_model} onChange={(event) => setForm({ ...form, upstream_model: event.target.value })} />
          </label>
          {editing && form.kind === "oauth_codex" ? <CodexOAuthPanel providerId={editing.id} /> : null}
          {error ? <p role="alert">{error}</p> : null}
          <button type="submit">Save provider</button>
        </form>
      ) : null}
    </section>
  );
}
```

Modify `frontend/src/App.tsx`:

```tsx
import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";
import { Providers } from "./pages/Providers";

function Placeholder({ title }: { title: string }) {
  return <h1>{title}</h1>;
}

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<Providers />} />
        <Route path="/ui/pools" element={<Placeholder title="Pools" />} />
        <Route path="/ui/settings" element={<Placeholder title="Settings" />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/pages/Providers.form.test.tsx`

Expected: PASS — provider list, state polling, create, edit, and delete tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/pages/Providers.tsx frontend/src/pages/Providers.form.test.tsx frontend/src/App.tsx
git commit -m "feat(ui): add providers admin page"
```

### Task C4b: Codex OAuth panel component

**Files:**
- Create: `frontend/src/components/CodexOAuthPanel.tsx`
- Create: `frontend/src/components/CodexOAuthPanel.test.tsx`

**Interfaces:**
- Consumes: C1/C2 and C4.
- Produces: two-step flow against existing `/admin/providers/:id/oauth/start` and `/admin/providers/:id/oauth/complete`.
- Produces: up-front copy above the Start button: `After you approve access, your browser will show a page that fails to load at localhost:1455 — that's expected. Copy the code value out of that page's address bar and paste it below.`

- [ ] **Step 1: Write the failing test**

Create `frontend/src/components/CodexOAuthPanel.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CodexOAuthPanel } from "./CodexOAuthPanel";

describe("CodexOAuthPanel", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/oauth/start" && init?.method === "POST") {
          return new Response(JSON.stringify({ authorize_url: "https://auth.example.test/start" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/oauth/complete" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    vi.stubGlobal("open", vi.fn());
  });

  it("shows_localhost_connection_error_copy_before_start", () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    expect(
      screen.getByText(
        "After you approve access, your browser will show a page that fails to load at localhost:1455 — that's expected. Copy the code value out of that page's address bar and paste it below."
      )
    ).toBeInTheDocument();
  });

  it("starts_oauth_and_opens_authorize_url", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.click(screen.getByRole("button", { name: "Start Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1/oauth/start", expect.objectContaining({ method: "POST" }));
    expect(window.open).toHaveBeenCalledWith("https://auth.example.test/start", "_blank", "noopener,noreferrer");
  });

  it("completes_oauth_with_pasted_code", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.type(screen.getByLabelText("Code"), "abc123");
    await userEvent.click(screen.getByRole("button", { name: "Complete Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/oauth/complete",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ code: "abc123" })
      })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Codex OAuth connected.");
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/components/CodexOAuthPanel.test.tsx`

Expected: FAIL — `Cannot find module './CodexOAuthPanel'`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/src/components/CodexOAuthPanel.tsx`:

```tsx
import { FormEvent, useState } from "react";
import { apiJson } from "../lib/apiClient";

type StartResponse = {
  authorize_url: string;
};

export function CodexOAuthPanel({ providerId }: { providerId: string }) {
  const [code, setCode] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function startOAuth() {
    setError(null);
    const body = await apiJson<StartResponse>(`/admin/providers/${providerId}/oauth/start`, { method: "POST" });
    window.open(body.authorize_url, "_blank", "noopener,noreferrer");
  }

  async function completeOAuth(event: FormEvent) {
    event.preventDefault();
    setError(null);
    setMessage(null);
    try {
      await apiJson(`/admin/providers/${providerId}/oauth/complete`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code })
      });
      setMessage("Codex OAuth connected.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Codex OAuth failed.");
    }
  }

  return (
    <section aria-labelledby="codex-oauth-title">
      <h2 id="codex-oauth-title">Codex OAuth</h2>
      <p>
        After you approve access, your browser will show a page that fails to load at localhost:1455 — that's expected.
        Copy the code value out of that page's address bar and paste it below.
      </p>
      <button type="button" onClick={startOAuth}>
        Start Codex OAuth
      </button>
      <form onSubmit={completeOAuth}>
        <label>
          Code
          <input value={code} onChange={(event) => setCode(event.target.value)} />
        </label>
        <button type="submit">Complete Codex OAuth</button>
      </form>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}
    </section>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/components/CodexOAuthPanel.test.tsx`

Expected: PASS — localhost copy, start, and complete tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/components/CodexOAuthPanel.tsx frontend/src/components/CodexOAuthPanel.test.tsx
git commit -m "feat(ui): add Codex OAuth panel"
```

### Task C5: Pools page

**Files:**
- Create: `frontend/src/pages/Pools.tsx`
- Create: `frontend/src/pages/Pools.reorder.test.tsx`
- Modify: `frontend/package.json`
- Modify: `frontend/src/App.tsx`

**Interfaces:**
- Consumes: C1/C2.
- Produces: create/delete pools.
- Produces: drag-to-reorder members persisted via `PUT /admin/pools/:id/members` with recomputed priorities.
- Produces: priority recompute logic unit-tested by C7: on drop, reassign `priority = index + 1` for the whole reordered array, not a delta patch, avoiding sparse-priority drift.
- Produces: `frontend/package.json` dependencies on `@dnd-kit/core` and `@dnd-kit/sortable`.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/pages/Pools.reorder.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Pools, recomputeMemberPriorities } from "./Pools";

describe("recomputeMemberPriorities", () => {
  it("priority_recompute_on_drag_reassigns_whole_reordered_array", () => {
    const result = recomputeMemberPriorities([
      { provider_id: "b", priority: 20 },
      { provider_id: "a", priority: 10 },
      { provider_id: "c", priority: 40 }
    ]);

    expect(result).toEqual([
      { provider_id: "b", priority: 1 },
      { provider_id: "a", priority: 2 },
      { provider_id: "c", priority: 3 }
    ]);
  });
});

describe("Pools", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              {
                id: "openai",
                wire_format: "openai",
                members: [
                  { provider_id: "a", provider_name: "alpha", priority: 1 },
                  { provider_id: "b", provider_name: "beta", priority: 2 }
                ]
              }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools" && init?.method === "POST") {
          return new Response(JSON.stringify({ id: "anthropic", wire_format: "anthropic", members: [] }), { status: 200 });
        }
        if (url === "/admin/pools/openai" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("creates_and_deletes_pools", async () => {
    render(<Pools />);

    await userEvent.type(await screen.findByLabelText("Pool id"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("Wire format"), "anthropic");
    await userEvent.click(screen.getByRole("button", { name: "Create pool" }));
    expect(fetch).toHaveBeenCalledWith("/admin/pools", expect.objectContaining({ method: "POST" }));

    await userEvent.click(screen.getByRole("button", { name: "Delete openai" }));
    expect(fetch).toHaveBeenCalledWith("/admin/pools/openai", expect.objectContaining({ method: "DELETE" }));
  });

  it("persists_reordered_members_with_dense_priorities", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Move beta up" }));
    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({
            members: [
              { provider_id: "b", priority: 1 },
              { provider_id: "a", priority: 2 }
            ]
          })
        })
      )
    );
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/pages/Pools.reorder.test.tsx`

Expected: FAIL — `Cannot find module './Pools'`.

- [ ] **Step 3: Write minimal implementation**

Modify `frontend/package.json`:

```json
{
  "name": "1router-admin-ui",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "scripts": {
    "build": "tsc -b && vite build",
    "dev": "vite",
    "test": "vitest run"
  },
  "dependencies": {
    "@dnd-kit/core": "^6.1.0",
    "@dnd-kit/sortable": "^8.0.0",
    "react": "^18.3.1",
    "react-dom": "^18.3.1",
    "react-router-dom": "^6.26.2"
  },
  "devDependencies": {
    "@vitejs/plugin-react": "^4.3.1",
    "typescript": "^5.6.2",
    "vite": "^5.4.8"
  }
}
```

Create `frontend/src/pages/Pools.tsx`:

```tsx
import { FormEvent, useEffect, useState } from "react";
import { DndContext, DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove } from "@dnd-kit/sortable";
import { apiJson } from "../lib/apiClient";

type PoolMember = {
  provider_id: string;
  provider_name?: string;
  priority: number;
};

type Pool = {
  id: string;
  wire_format: string;
  members: PoolMember[];
};

export function recomputeMemberPriorities(members: PoolMember[]) {
  return members.map((member, index) => ({
    provider_id: member.provider_id,
    priority: index + 1
  }));
}

export function Pools() {
  const [pools, setPools] = useState<Pool[]>([]);
  const [poolId, setPoolId] = useState("");
  const [wireFormat, setWireFormat] = useState("openai");
  const [error, setError] = useState<string | null>(null);

  async function loadPools() {
    setPools(await apiJson<Pool[]>("/admin/pools"));
  }

  useEffect(() => {
    void loadPools();
  }, []);

  async function createPool(event: FormEvent) {
    event.preventDefault();
    await apiJson("/admin/pools", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id: poolId, wire_format: wireFormat })
    });
    setPoolId("");
    await loadPools();
  }

  async function deletePool(id: string) {
    await apiJson(`/admin/pools/${id}`, { method: "DELETE" });
    setPools((current) => current.filter((pool) => pool.id !== id));
  }

  async function persistMembers(poolId: string, members: PoolMember[]) {
    await apiJson(`/admin/pools/${poolId}/members`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ members: recomputeMemberPriorities(members) })
    });
  }

  async function moveMember(pool: Pool, providerId: string, direction: -1 | 1) {
    const oldIndex = pool.members.findIndex((member) => member.provider_id === providerId);
    const newIndex = oldIndex + direction;
    if (oldIndex < 0 || newIndex < 0 || newIndex >= pool.members.length) {
      return;
    }
    const members = arrayMove(pool.members, oldIndex, newIndex);
    setPools((current) => current.map((item) => (item.id === pool.id ? { ...item, members } : item)));
    await persistMembers(pool.id, members);
  }

  async function onDragEnd(pool: Pool, event: DragEndEvent) {
    if (!event.over || event.active.id === event.over.id) {
      return;
    }
    const oldIndex = pool.members.findIndex((member) => member.provider_id === event.active.id);
    const newIndex = pool.members.findIndex((member) => member.provider_id === event.over?.id);
    const members = arrayMove(pool.members, oldIndex, newIndex);
    setPools((current) => current.map((item) => (item.id === pool.id ? { ...item, members } : item)));
    try {
      await persistMembers(pool.id, members);
    } catch (error) {
      setError(error instanceof Error ? error.message : "Pool reorder failed.");
    }
  }

  return (
    <section aria-labelledby="pools-title">
      <h1 id="pools-title">Pools</h1>
      <form onSubmit={createPool}>
        <label>
          Pool id
          <input value={poolId} onChange={(event) => setPoolId(event.target.value)} />
        </label>
        <label>
          Wire format
          <select value={wireFormat} onChange={(event) => setWireFormat(event.target.value)}>
            <option value="openai">openai</option>
            <option value="anthropic">anthropic</option>
          </select>
        </label>
        <button type="submit">Create pool</button>
      </form>
      {error ? <p role="alert">{error}</p> : null}
      {pools.map((pool) => (
        <section key={pool.id} aria-label={`Pool ${pool.id}`}>
          <h2>{pool.id}</h2>
          <p>{pool.wire_format}</p>
          <button type="button" onClick={() => deletePool(pool.id)} aria-label={`Delete ${pool.id}`}>
            Delete
          </button>
          <DndContext onDragEnd={(event) => onDragEnd(pool, event)}>
            <SortableContext items={pool.members.map((member) => member.provider_id)}>
              <ol>
                {pool.members.map((member, index) => (
                  <li key={member.provider_id}>
                    <span>{member.provider_name ?? member.provider_id}</span>
                    <button type="button" onClick={() => moveMember(pool, member.provider_id, -1)} aria-label={`Move ${member.provider_name ?? member.provider_id} up`} disabled={index === 0}>
                      Up
                    </button>
                    <button type="button" onClick={() => moveMember(pool, member.provider_id, 1)} aria-label={`Move ${member.provider_name ?? member.provider_id} down`} disabled={index === pool.members.length - 1}>
                      Down
                    </button>
                  </li>
                ))}
              </ol>
            </SortableContext>
          </DndContext>
        </section>
      ))}
    </section>
  );
}
```

Modify `frontend/src/App.tsx`:

```tsx
import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";
import { Pools } from "./pages/Pools";
import { Providers } from "./pages/Providers";

function Placeholder({ title }: { title: string }) {
  return <h1>{title}</h1>;
}

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<Providers />} />
        <Route path="/ui/pools" element={<Pools />} />
        <Route path="/ui/settings" element={<Placeholder title="Settings" />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/pages/Pools.reorder.test.tsx`

Expected: PASS — create/delete and dense whole-array priority recompute tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/package.json frontend/src/pages/Pools.tsx frontend/src/pages/Pools.reorder.test.tsx frontend/src/App.tsx
git commit -m "feat(ui): add pools admin page"
```

### Task C6: Settings page

**Files:**
- Create: `frontend/src/pages/Settings.tsx`
- Create: `frontend/src/pages/Settings.test.tsx`
- Modify: `frontend/src/App.tsx`

**Interfaces:**
- Consumes: C2.
- Produces: password change via `PATCH /admin/auth/password` with `{ current_password, new_password }`.
- Produces: shared secret view/edit via `GET/PATCH /admin/settings/shared-secret`.
- Produces: `GET /admin/settings/shared-secret?reveal=true` for the real value, otherwise masked.
- Produces: `PATCH /admin/settings/shared-secret` handling where 409 renders the server message verbatim rather than a generic error.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/pages/Settings.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Settings } from "./Settings";

describe("Settings", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/settings/shared-secret") {
          return new Response(JSON.stringify({ shared_secret: "sec_****" }), { status: 200 });
        }
        if (url === "/admin/settings/shared-secret?reveal=true") {
          return new Response(JSON.stringify({ shared_secret: "sec_real" }), { status: 200 });
        }
        if (url === "/admin/auth/password" && init?.method === "PATCH") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/settings/shared-secret" && init?.method === "PATCH") {
          return new Response(JSON.stringify({ error: { message: "ROUTER_SHARED_SECRET is set; change it there" } }), {
            status: 409
          });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("loads_masked_secret_and_reveals_real_value", async () => {
    render(<Settings />);

    expect(await screen.findByDisplayValue("sec_****")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByDisplayValue("sec_real")).toBeInTheDocument();
  });

  it("changes_admin_password", async () => {
    render(<Settings />);

    await userEvent.type(screen.getByLabelText("Current password"), "old");
    await userEvent.type(screen.getByLabelText("New password"), "new-secret");
    await userEvent.click(screen.getByRole("button", { name: "Change password" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/auth/password",
      expect.objectContaining({
        method: "PATCH",
        body: JSON.stringify({ current_password: "old", new_password: "new-secret" })
      })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Password changed.");
  });

  it("renders_shared_secret_409_message_verbatim", async () => {
    render(<Settings />);

    await userEvent.clear(await screen.findByLabelText("Shared secret"));
    await userEvent.type(screen.getByLabelText("Shared secret"), "replacement");
    await userEvent.click(screen.getByRole("button", { name: "Save shared secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("ROUTER_SHARED_SECRET is set; change it there");
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npx vitest run src/pages/Settings.test.tsx`

Expected: FAIL — `Cannot find module './Settings'`.

- [ ] **Step 3: Write minimal implementation**

Create `frontend/src/pages/Settings.tsx`:

```tsx
import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";

// Assembly note: B7's canonical response also carries `masked`/`origin` -
// harmless to ignore for the fields already used below, declared here for
// type accuracy (e.g. to skip the "Reveal" round-trip when already unmasked).
type SharedSecretResponse = {
  shared_secret: string;
  masked: boolean;
  origin: "env" | "sidecar_file";
};

export function Settings() {
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [sharedSecret, setSharedSecret] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function loadSharedSecret(reveal = false) {
    const suffix = reveal ? "?reveal=true" : "";
    const body = await apiJson<SharedSecretResponse>(`/admin/settings/shared-secret${suffix}`);
    setSharedSecret(body.shared_secret);
  }

  useEffect(() => {
    void loadSharedSecret(false);
  }, []);

  async function changePassword(event: FormEvent) {
    event.preventDefault();
    setMessage(null);
    setError(null);
    try {
      await apiJson("/admin/auth/password", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ current_password: currentPassword, new_password: newPassword })
      });
      setCurrentPassword("");
      setNewPassword("");
      setMessage("Password changed.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Password change failed.");
    }
  }

  async function saveSharedSecret(event: FormEvent) {
    event.preventDefault();
    setMessage(null);
    setError(null);
    try {
      await apiJson("/admin/settings/shared-secret", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ shared_secret: sharedSecret })
      });
      setMessage("Shared secret saved.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Shared secret save failed.");
    }
  }

  return (
    <section aria-labelledby="settings-title">
      <h1 id="settings-title">Settings</h1>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}

      <form onSubmit={changePassword}>
        <h2>Admin password</h2>
        <label>
          Current password
          <input type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} />
        </label>
        <label>
          New password
          <input type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} />
        </label>
        <button type="submit">Change password</button>
      </form>

      <form onSubmit={saveSharedSecret}>
        <h2>Shared secret</h2>
        <label>
          Shared secret
          <input value={sharedSecret} onChange={(event) => setSharedSecret(event.target.value)} />
        </label>
        <button type="button" onClick={() => loadSharedSecret(true)}>
          Reveal shared secret
        </button>
        <button type="submit">Save shared secret</button>
      </form>
    </section>
  );
}
```

Modify `frontend/src/App.tsx`:

```tsx
import { Navigate, NavLink, Route, Routes } from "react-router-dom";
import { Login } from "./pages/Login";
import { Pools } from "./pages/Pools";
import { Providers } from "./pages/Providers";
import { Settings } from "./pages/Settings";

export function App() {
  return (
    <main>
      <nav aria-label="Admin sections">
        <NavLink to="/ui/providers">Providers</NavLink>
        <NavLink to="/ui/pools">Pools</NavLink>
        <NavLink to="/ui/settings">Settings</NavLink>
      </nav>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<Providers />} />
        <Route path="/ui/pools" element={<Pools />} />
        <Route path="/ui/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/ui/providers" replace />} />
      </Routes>
    </main>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npx vitest run src/pages/Settings.test.tsx`

Expected: PASS — masked load, reveal, password change, and verbatim 409 message tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/pages/Settings.tsx frontend/src/pages/Settings.test.tsx frontend/src/App.tsx
git commit -m "feat(ui): add settings page"
```

### Task C7: Vitest + RTL test setup

**Files:**
- Create: `frontend/vitest.config.ts`
- Create: `frontend/src/test/setup.ts`
- Modify: `frontend/package.json`
- Modify: `frontend/tsconfig.json`
- Create: test files alongside each page/lib module as they land:
  - `frontend/src/lib/apiClient.test.ts`
  - `frontend/src/pages/Pools.reorder.test.tsx`
  - `frontend/src/pages/Login.test.tsx`
  - `frontend/src/pages/Providers.form.test.tsx`

**Interfaces:**
- Consumes: C1 tooling, then C2/C4/C5/C6 incrementally.
- Produces: Vitest + React Testing Library setup for unit/component tests.
- Produces: key test names: `apiClient.test.ts`, `Pools.reorder.test.tsx`, `Login.test.tsx`, `Providers.form.test.tsx`.
- Produces: C2 required named tests: `apiFetch_sets_csrf_header_on_post_but_not_get`, `apiFetch_redirects_to_login_on_401_unless_already_on_login_page`, `apiJson_throws_with_server_error_message_on_non_ok`.
- Produces: no e2e/browser suite for v1.

- [ ] **Step 1: Write the failing test**

Create `frontend/src/test/setup.ts`:

```ts
import "@testing-library/jest-dom/vitest";
```

Create `frontend/vitest.config.ts`:

```ts
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    restoreMocks: true
  }
});
```

Create or keep `frontend/src/lib/apiClient.test.ts`:

```ts
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { apiFetch, apiJson } from "./apiClient";

describe("apiClient", () => {
  const originalLocation = window.location;

  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 200 })));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    Object.defineProperty(window, "location", { configurable: true, value: originalLocation });
  });

  it("apiFetch_sets_csrf_header_on_post_but_not_get", async () => {
    await apiFetch("/admin/providers");
    await apiFetch("/admin/providers", { method: "POST" });

    const fetchMock = vi.mocked(fetch);
    expect(new Headers(fetchMock.mock.calls[0][1]?.headers).get("X-Requested-With")).toBeNull();
    expect(new Headers(fetchMock.mock.calls[1][1]?.headers).get("X-Requested-With")).toBe("1router-ui");
  });

  it("apiFetch_redirects_to_login_on_401_unless_already_on_login_page", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 401 })));
    const assign = vi.fn();
    Object.defineProperty(window, "location", { configurable: true, value: { pathname: "/ui/providers", assign } });
    await apiFetch("/admin/providers");
    expect(assign).toHaveBeenCalledWith("/ui/login");

    assign.mockClear();
    Object.defineProperty(window, "location", { configurable: true, value: { pathname: "/ui/login", assign } });
    await apiFetch("/admin/providers");
    expect(assign).not.toHaveBeenCalled();
  });

  it("apiJson_throws_with_server_error_message_on_non_ok", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ error: { message: "bad password" } }), { status: 401 })));

    await expect(apiJson("/admin/auth/login", { method: "POST" })).rejects.toThrow("bad password");
  });
});
```

Create or keep `frontend/src/pages/Pools.reorder.test.tsx`:

```tsx
import { describe, expect, it } from "vitest";
import { recomputeMemberPriorities } from "./Pools";

describe("recomputeMemberPriorities", () => {
  it("priority_recompute_on_drag_reassigns_whole_reordered_array", () => {
    expect(
      recomputeMemberPriorities([
        { provider_id: "later", priority: 50 },
        { provider_id: "first", priority: 10 }
      ])
    ).toEqual([
      { provider_id: "later", priority: 1 },
      { provider_id: "first", priority: 2 }
    ]);
  });
});
```

Create or keep `frontend/src/pages/Login.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { Login } from "./Login";

describe("Login", () => {
  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn());
  });

  it("form_validation_blocks_empty_submit", async () => {
    render(
      <MemoryRouter initialEntries={["/ui/login"]}>
        <Routes>
          <Route path="/ui/login" element={<Login />} />
        </Routes>
      </MemoryRouter>
    );

    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Username and password are required.");
  });
});
```

Create or keep `frontend/src/pages/Providers.form.test.tsx`:

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { Providers } from "./Providers";

describe("Providers", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        if (String(input) === "/admin/providers") {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        return new Response(JSON.stringify({ state: "ready" }), { status: 200 });
      })
    );
  });

  it("renders_provider_form_entrypoint", async () => {
    render(<Providers />);
    expect(await screen.findByRole("button", { name: "New provider" })).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd frontend && npm test`

Expected: FAIL — `Cannot find package 'vitest'` or `Cannot find package '@testing-library/react'`.

- [ ] **Step 3: Write minimal implementation**

Modify `frontend/package.json`:

```json
{
  "name": "1router-admin-ui",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "scripts": {
    "build": "tsc -b && vite build",
    "dev": "vite",
    "test": "vitest run"
  },
  "dependencies": {
    "@dnd-kit/core": "^6.1.0",
    "@dnd-kit/sortable": "^8.0.0",
    "react": "^18.3.1",
    "react-dom": "^18.3.1",
    "react-router-dom": "^6.26.2"
  },
  "devDependencies": {
    "@testing-library/jest-dom": "^6.4.8",
    "@testing-library/react": "^16.0.1",
    "@testing-library/user-event": "^14.5.2",
    "@vitejs/plugin-react": "^4.3.1",
    "jsdom": "^25.0.1",
    "typescript": "^5.6.2",
    "vite": "^5.4.8",
    "vitest": "^2.1.1"
  }
}
```

Modify `frontend/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "useDefineForClassFields": true,
    "lib": ["DOM", "DOM.Iterable", "ES2020"],
    "allowJs": false,
    "skipLibCheck": true,
    "esModuleInterop": true,
    "allowSyntheticDefaultImports": true,
    "strict": true,
    "forceConsistentCasingInFileNames": true,
    "module": "ESNext",
    "moduleResolution": "Node",
    "resolveJsonModule": true,
    "isolatedModules": true,
    "noEmit": true,
    "jsx": "react-jsx",
    "types": ["vitest/globals", "@testing-library/jest-dom"]
  },
  "include": ["src", "vite.config.ts", "vitest.config.ts"]
}
```

Keep `frontend/vitest.config.ts` and `frontend/src/test/setup.ts` exactly as written in Step 1. Keep no Playwright/Cypress/browser e2e configuration for v1.

- [ ] **Step 4: Run to verify it passes**

Run: `cd frontend && npm test`

Expected: PASS — Vitest runs in jsdom and passes `apiClient.test.ts`, `Pools.reorder.test.tsx`, `Login.test.tsx`, and `Providers.form.test.tsx`.

- [ ] **Step 5: Commit**

```bash
git add frontend/package.json frontend/tsconfig.json frontend/vitest.config.ts frontend/src/test/setup.ts frontend/src/lib/apiClient.test.ts frontend/src/pages/Pools.reorder.test.tsx frontend/src/pages/Login.test.tsx frontend/src/pages/Providers.form.test.tsx
git commit -m "test(ui): add Vitest React Testing Library setup"
```

---

## Phase D — Build / CI / Docker integration

**Parallelism:** D1 needs C1 (frontend scaffold must exist for the build script to have something to build). D2 needs A2 (Cargo feature plumbing) + D1. D3 needs D1 + C1. None of D1–D3 parallelize with each other — sequence D1 → D2 → D3.

### Task D1: `build.rs`

**Files:**
- Create: `build.rs`

**Interfaces:**
- Consumes: C1’s `frontend/package.json`, `frontend/package-lock.json`, `frontend/src/`, and `npm run build` contract producing `frontend/dist/index.html`.
- Consumes: A2’s default-on Cargo `ui` feature. The build script must detect it via `CARGO_FEATURE_UI`, not `cfg!(feature = "ui")`.
- Produces: a Cargo build script that:
  - does nothing when `ui` is disabled;
  - does nothing when `frontend/dist/index.html` already exists;
  - otherwise verifies `node` and `npm` are on `PATH`;
  - runs `npm ci` then `npm run build` from `frontend/`.

- [ ] **Step 1: Write the failing test**

No Rust unit test is appropriate for a Cargo build script. Use a shell verification that proves a default-feature build invokes `npm ci` and `npm run build` when `frontend/dist/index.html` is missing.

Run from repo root after C1 and A2 have landed:

```bash
rm -rf frontend/dist
marker_dir="$(mktemp -d)"
bin_dir="$(mktemp -d)"

cat > "$bin_dir/node" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$bin_dir/node"

cat > "$bin_dir/npm" <<'EOF'
#!/bin/sh
set -eu
case "$*" in
  ci)
    touch "$MARKER_DIR/npm-ci"
    ;;
  "run build")
    touch "$MARKER_DIR/npm-run-build"
    mkdir -p dist
    printf '<!doctype html><div id="root"></div>\n' > dist/index.html
    ;;
  *)
    echo "unexpected npm args: $*" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$bin_dir/npm"

MARKER_DIR="$marker_dir" PATH="$bin_dir:$PATH" cargo build --offline
test -f "$marker_dir/npm-ci"
test -f "$marker_dir/npm-run-build"
test -f frontend/dist/index.html
```

- [ ] **Step 2: Run to verify it fails**

Run the Step 1 script before `build.rs` exists.

Expected: FAIL after `cargo build --offline`, because Cargo does not run any frontend build yet and the marker assertions fail:

```text
test: .../npm-ci: No such file or directory
```

If D2 has already landed accidentally, an alternate pre-D1 failure is acceptable:

```text
folder 'frontend/dist/' does not exist
```

That means `rust-embed` tried to embed missing assets before the build script created them.

- [ ] **Step 3: Write minimal implementation**

Create `build.rs`:

```rust
fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");

    // Build scripts see feature activation via CARGO_FEATURE_<NAME>, not
    // cfg!(feature = "..."). cfg!() reflects build.rs's own compilation unit.
    let ui_enabled = std::env::var("CARGO_FEATURE_UI").is_ok();
    if !ui_enabled {
        return;
    }

    if std::path::Path::new("frontend/dist/index.html").exists() {
        return;
    }

    for bin in ["node", "npm"] {
        if which(bin).is_none() {
            panic!(
                "the `ui` feature is enabled but `{bin}` was not found on PATH; \
                 install Node.js, or build with `--no-default-features` to skip \
                 the embedded admin UI entirely"
            );
        }
    }

    run("npm", &["ci"]);
    run("npm", &["run", "build"]);
}

fn run(cmd: &str, args: &[&str]) {
    let status = std::process::Command::new(cmd)
        .args(args)
        .current_dir("frontend")
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd} {}`: {e}", args.join(" ")));

    assert!(status.success(), "`{cmd} {}` failed", args.join(" "));
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file().then_some(candidate)
        })
    })
}
```

- [ ] **Step 4: Run to verify it passes**

Run the Step 1 shell verification again.

Expected: PASS. The command creates:

```text
frontend/dist/index.html
```

and both marker files exist.

Then run the feature-off regression check:

```bash
rm -rf frontend/dist
cargo build --offline --no-default-features
```

Expected: PASS, with no `npm` invocation.

Then verify the existing-dist short circuit:

```bash
mkdir -p frontend/dist
printf '<!doctype html><div id="root"></div>\n' > frontend/dist/index.html
cargo build --offline
```

Expected: PASS without running `npm ci` or `npm run build`.

- [ ] **Step 5: Commit**

```bash
git add build.rs
git commit -m "feat: build.rs shells out to npm ci/build when ui feature is on and dist/ is missing"
```

### Task D2: `rust-embed` static asset module

**Files:**
- Create: `src/ui_assets.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml` only if A2 did not already add the exact optional dependency and feature block.

**Interfaces:**
- Consumes: A2’s Cargo feature plumbing:

```toml
rust-embed = { version = "8", optional = true, features = ["mime-guess"] }

[features]
default = ["ui"]
ui = ["dep:rust-embed"]
```

- Consumes: D1’s guarantee that `frontend/dist/index.html` exists before a default-feature build reaches `rust-embed`.
- Produces:

```rust
pub fn routes() -> Router<AppState>;
```

Routes:
- `GET /ui` -> permanent redirect to `/ui/` (`308`)
- `GET /ui/*path` -> embedded asset lookup using axum 0.7’s named wildcard syntax
- unknown `/ui/...` paths -> `index.html` SPA fallback

- [ ] **Step 1: Write the failing test**

Add the module declaration to `src/lib.rs`:

```rust
#[cfg(feature = "ui")]
pub mod ui_assets;
```

Create `src/ui_assets.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn routes_registers_axum_07_named_wildcard_without_panicking() {
        let _ = super::routes();
    }

    #[tokio::test]
    async fn redirect_returns_308_to_ui_slash() {
        let resp = super::redirect_to_ui_slash().await.into_response();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }

    #[tokio::test]
    async fn serve_asset_serves_index_html_at_root() {
        let resp = super::serve_asset(Path(String::new())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&body).unwrap().contains("<"));
    }

    #[tokio::test]
    async fn serve_asset_falls_back_to_index_for_unknown_subpath() {
        let resp = super::serve_asset(Path("providers/deep-link".to_string())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));
    }

    #[tokio::test]
    async fn ui_route_reaches_redirect_handler() {
        let app = super::routes();

        let resp = app
            .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Ensure `frontend/dist/index.html` exists first, either from C1’s real build:

```bash
cd frontend && npm ci && npm run build
```

or for isolated Rust iteration:

```bash
mkdir -p frontend/dist
printf '<!doctype html><div id="root"></div>\n' > frontend/dist/index.html
```

Then run:

```bash
cargo test --lib ui_assets
```

Expected: FAIL with missing items from the test-first file:

```text
error[E0425]: cannot find function `routes` in module `super`
error[E0425]: cannot find function `redirect_to_ui_slash` in module `super`
error[E0425]: cannot find function `serve_asset` in module `super`
```

- [ ] **Step 3: Write minimal implementation**

Replace `src/ui_assets.rs` with:

```rust
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::core::state::AppState;

#[derive(rust_embed::RustEmbed)]
#[folder = "frontend/dist/"]
struct Dist;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui", get(redirect_to_ui_slash))
        .route("/ui/*path", get(serve_asset))
}

async fn redirect_to_ui_slash() -> Redirect {
    Redirect::permanent("/ui/")
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    let path = path.trim_start_matches('/');
    let lookup = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Dist::get(lookup) {
        let mime = file.metadata.mimetype();
        return ([(header::CONTENT_TYPE, mime)], file.data).into_response();
    }

    match Dist::get("index.html") {
        Some(file) => ([(header::CONTENT_TYPE, "text/html")], file.data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn routes_registers_axum_07_named_wildcard_without_panicking() {
        let _ = super::routes();
    }

    #[tokio::test]
    async fn redirect_returns_308_to_ui_slash() {
        let resp = super::redirect_to_ui_slash().await.into_response();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }

    #[tokio::test]
    async fn serve_asset_serves_index_html_at_root() {
        let resp = super::serve_asset(Path(String::new())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(std::str::from_utf8(&body).unwrap().contains("<"));
    }

    #[tokio::test]
    async fn serve_asset_falls_back_to_index_for_unknown_subpath() {
        let resp = super::serve_asset(Path("providers/deep-link".to_string())).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get("content-type").unwrap();
        assert!(content_type.to_str().unwrap().starts_with("text/html"));
    }

    #[tokio::test]
    async fn ui_route_reaches_redirect_handler() {
        let app = super::routes();

        let resp = app
            .oneshot(Request::builder().uri("/ui").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/");
    }
}
```

If A2 did not already land, update `Cargo.toml` exactly as A2 specified:

```toml
rust-embed = { version = "8", optional = true, features = ["mime-guess"] }

[features]
default = ["ui"]
ui = ["dep:rust-embed"]
```

- [ ] **Step 4: Run to verify it passes**

With `frontend/dist/index.html` present:

```bash
cargo test --lib ui_assets
```

Expected: PASS, 5 tests.

Then verify the module compiles out completely for Codex/offline work:

```bash
rm -rf frontend/dist
cargo build --offline --no-default-features
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui_assets.rs src/lib.rs Cargo.toml
git commit -m "feat: rust-embed static asset serving for /ui/*"
```

### Task D3: CI + Docker wiring

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `Dockerfile`

**Interfaces:**
- Consumes: C1’s committed `frontend/package.json` and `frontend/package-lock.json`.
- Consumes: D1’s build script, which runs `npm ci && npm run build` during `cargo build --release` when needed.
- Produces:
  - GitHub release binary matrix jobs with Node.js available before `cargo build --release`.
  - Docker builder image with `nodejs` and `npm`.
  - Docker layer order that caches `npm ci` on `frontend/package*.json`, then rebuilds frontend assets before Rust source is copied.

- [ ] **Step 1: Write the failing verification**

Run these checks before editing:

```bash
grep -n "actions/setup-node@v4" .github/workflows/release.yml
grep -n "nodejs npm" Dockerfile
grep -n "COPY frontend/package\\*.json frontend/" Dockerfile
grep -n "RUN cd frontend && npm ci" Dockerfile
grep -n "COPY build.rs ./build.rs" Dockerfile
```

- [ ] **Step 2: Run to verify it fails**

Expected: FAIL. In the current checkout, the release workflow has no Node setup step, and the Dockerfile builder stage only installs Rust/sqlite build dependencies:

```text
grep: no matches found
```

or exit code `1` from each missing-pattern check.

- [ ] **Step 3: Write minimal implementation**

In `.github/workflows/release.yml`, add this step after `Checkout` and before `Install musl build tools`:

```yaml
      - name: Set up Node
        uses: actions/setup-node@v4
        with:
          node-version: '22'
```

The `binaries` job should now have this order:

```yaml
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Set up Node
        uses: actions/setup-node@v4
        with:
          node-version: '22'

      - name: Install musl build tools (Linux legs only)
        if: startsWith(matrix.runner, 'ubuntu')
        run: |
          sudo apt-get update
          sudo apt-get install -y musl-tools

      - name: Add Rust target
        run: rustup target add ${{ matrix.target }}

      - name: Build release binary
        run: cargo build --release --target ${{ matrix.target }}
```

In `Dockerfile`, replace the builder stage with:

```dockerfile
# ---- build stage ----
FROM rust:1.90-alpine AS builder
# rustls (not openssl) handles TLS and sqlx's sqlite feature bundles/statically
# links libsqlite3, so no OpenSSL dependency is actually needed at build or
# runtime - musl-dev + sqlite-static is sufficient. The embedded admin UI adds
# Node/npm as build-time-only dependencies.
RUN apk add --no-cache musl-dev sqlite-static pkgconfig nodejs npm
WORKDIR /app

COPY frontend/package*.json frontend/
RUN cd frontend && npm ci
COPY frontend/ frontend/
RUN cd frontend && npm run build

COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY migrations ./migrations
COPY build.rs ./build.rs
COPY src ./src
# TARGETARCH is supplied by buildx (docker/amd64 -> "amd64", docker/arm64 ->
# "arm64"). Map it to the matching Rust musl triple, then stage the binary at a
# fixed, arch-independent path so the runtime-stage COPY needs no arch in it
# (COPY cannot run shell, so it cannot do the mapping itself).
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    cargo build --release --target "$RUST_TARGET"; \
    cp "target/${RUST_TARGET}/release/1router" /app/1router
```

Leave the runtime stage unchanged.

- [ ] **Step 4: Run to verify it passes**

Static checks:

```bash
grep -n "actions/setup-node@v4" .github/workflows/release.yml
grep -n "node-version: '22'" .github/workflows/release.yml
grep -n "nodejs npm" Dockerfile
grep -n "COPY frontend/package\\*.json frontend/" Dockerfile
grep -n "RUN cd frontend && npm ci" Dockerfile
grep -n "COPY frontend/ frontend/" Dockerfile
grep -n "RUN cd frontend && npm run build" Dockerfile
grep -n "COPY build.rs ./build.rs" Dockerfile
```

Expected: PASS, each command prints one matching line.

Local Docker verification after C1/D1/D2 have landed:

```bash
docker build --build-arg TARGETARCH=amd64 -t 1router-admin-ui-smoke .
docker run --rm -p 18080:8080 \
  -e ROUTER_SHARED_SECRET=test-secret \
  1router-admin-ui-smoke
```

In another shell:

```bash
curl -i http://127.0.0.1:18080/ui/
```

Expected: `200 OK` with an HTML SPA shell.

Also check redirect:

```bash
curl -i http://127.0.0.1:18080/ui
```

Expected: `308 Permanent Redirect` with:

```text
location: /ui/
```

Workflow verification, outside Codex:

```bash
act push -W .github/workflows/release.yml
```

Expected: the `binaries` job reaches `cargo build --release` with Node available. If `act` is not configured for the macOS matrix, push a throwaway tag to a fork and verify all four matrix legs pass.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml Dockerfile
git commit -m "ci: wire Node/npm into release binaries + Docker builder for the embedded admin UI"
```

---

## Phase E — Integration / wiring

**Parallelism:** None of E1–E4 parallelize with each other or with upstream. Dispatch only after every task in A/B/D has landed.

### Task E1: `app.rs` router stratification (solo, last)

**Files:** Modify `src/app.rs` (the real `build_router` rewrite)

**Interfaces:** Consumes everything upstream (A4, B3, B4, B5, B7, D2).

```rust
use axum::Router;
use crate::auth::middleware::{require_admin_session, require_bearer, require_csrf_header};
use crate::core::state::AppState;

pub fn build_router(state: AppState) -> Router {
    // Authenticated admin surface: everything admin-side except login.
    let admin_authenticated = Router::new()
        .merge(crate::telemetry::stats::routes())
        .merge(crate::providers::routes())
        .merge(crate::providers::oauth_routes::routes())
        .merge(crate::pools::routes::routes())
        .merge(crate::admin::routes())              // export/import
        .merge(crate::admin::auth::routes())          // logout, password-change
        .merge(crate::admin::settings::routes())      // shared-secret settings
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_admin_session));

    // Unauthenticated admin surface: login only (spec's I2 fix — login can't gate itself).
    let admin_public = crate::admin::auth::public_routes();

    // CSRF applies across BOTH admin strata — login is itself a POST.
    let admin = Router::new()
        .merge(admin_authenticated)
        .merge(admin_public)
        .layer(axum::middleware::from_fn(require_csrf_header));

    // /v1/* stays exactly as-is: require_bearer only, no cookie fallback.
    let proxy = crate::proxy::routes::routes()
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_bearer));

    let mut router = Router::new()
        .merge(crate::telemetry::health::routes())
        .merge(admin)
        .merge(proxy);

    #[cfg(feature = "ui")]
    {
        router = router.merge(crate::ui_assets::routes()); // unauthenticated SPA shell
    }

    router.with_state(state)
}
```
Layering discipline note (so this doesn't regress): apply `.route_layer()`/`.layer()` to a sub-router **before** merging it upward, never after — the existing pre-E1 code already follows this discipline (`guarded.route_layer(...)` then merged into the outer `Router::new()`); E1 just splits "guarded" into two admin strata + proxy along the same lines.

`test_state()` in `src/app.rs` (already carries A4's/B2's fields from those tasks — no further field additions needed here, only new route-level tests below).

Tests:
- `health_still_unauthenticated`
- `admin_login_reachable_without_any_auth` (public stratum, but still requires the CSRF header since it's a POST)
- `admin_providers_requires_auth_401_with_neither_cookie_nor_bearer`
- `admin_providers_accepts_bearer_still` (non-breaking curl/CI fix, I1)
- `admin_providers_accepts_session_cookie`
- `v1_models_requires_bearer_only_cookie_alone_is_insufficient` (proves the isolation the spec demands — a valid admin session cookie must NOT authenticate `/v1/*`)
- `csrf_blocks_post_admin_auth_login_without_header`
- `ui_route_reachable_without_auth` (only compiled/run with the `ui` feature on)

- [ ] **Step 2:** `cargo test --offline --lib app::tests` → several new tests FAIL against the pre-E1 single-`guarded` router.
- [ ] **Step 4:** PASS, all 8 (7 without `ui` feature) tests, plus full `cargo test --offline` and `cargo test --offline --no-default-features` both green.
- [ ] **Step 5:** `git add src/app.rs && git commit -m "feat: stratify /admin/* into public (login) and session-authenticated routers, wire CSRF + UI asset routes"`

### Task E2: `main.rs`/`tests/common` final reconciliation

**Files:** Modify `src/main.rs`, `tests/common/mod.rs`

**Interfaces:** Consumes E1. Verifies the fully-composed boot order: secret resolution → admin bootstrap (A6) → `AppState` construction (`shared_secret`/`secret_origin` from A4, `login_attempts` from B2) → boot-time `delete_expired` + `spawn_background_refresh` + `spawn_session_cleanup` (B6) → `into_make_service_with_connect_info` (B2) → `build_router` (E1).

**Additional fix required here (correction #7):** `tests/common::spawn_app_with_sqlite_path` has no deterministic admin credential — needed by E3's integration test. Add:
```rust
pub struct TestApp {
    pub base_url: String,
    pub secret: String,
    pub admin_password: String,   // new
    pub db: SqlitePool,
}
```
and, after `init_pool`, seed a known admin row directly (bypassing A6's TTY/headless bootstrap, which is unsuitable for tests):
```rust
let admin_password = "test-admin-password".to_string();
let hash = router::admin::auth::password::hash_password(&admin_password).unwrap();
sqlx::query("INSERT INTO admin_users (id, username, password_hash, updated_at) VALUES (1, 'admin', ?, ?)")
    .bind(&hash)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&db).await.unwrap();
```

- [ ] **Step 2:** `cargo build --offline` (whole workspace incl. `tests/`) → FAIL if any upstream task's edits composed incorrectly (e.g. wrong field order, a missed `main.rs` sequencing point).
- [ ] **Step 4:** `cargo build --offline`, `cargo build --offline --no-default-features`, and `cargo test --offline --lib` (whole suite) all pass.
- [ ] **Step 5:** `git add src/main.rs tests/common/mod.rs && git commit -m "chore: reconcile main.rs/tests/common boot order across A6/B2/B6, seed deterministic admin_users row for tests"`

### Task E3: End-to-end integration test (manual verification required)

**Files:** Create `tests/admin_ui_flow.rs`

**Interfaces:** Consumes E1/E2.

```rust
mod common;

#[tokio::test]
async fn login_then_authenticated_providers_call_succeeds() {
    let app = common::spawn_app().await;
    let client = reqwest::Client::builder().cookie_store(true).build().unwrap();

    let login = client.post(format!("{}/admin/auth/login", app.base_url))
        .header("X-Requested-With", "1router-ui")
        .json(&serde_json::json!({"username": "admin", "password": app.admin_password}))
        .send().await.unwrap();
    assert_eq!(login.status(), 200);

    let providers = client.get(format!("{}/admin/providers", app.base_url))
        .send().await.unwrap();
    assert_eq!(providers.status(), 200); // cookie alone authenticates, no Bearer header sent
}
```

**Per `CLAUDE.md`: this binds a real TCP listener via `spawn_app` and will report BLOCKED in the Codex sandbox even if correct — must be run manually outside the sandbox.**

- [ ] **Step 2:** `cargo test --offline --test admin_ui_flow` (run outside Codex, real machine) → FAIL before E1/E2 land.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** `git add tests/admin_ui_flow.rs && git commit -m "test: end-to-end login -> cookie -> authenticated /admin/providers call"`

### Task E4: Docs addendum

**Files:** Modify `CLAUDE.md`

Add to the existing "Known Codex sandbox limitations" section:
> Once the `ui` feature is default-on, every `cargo build --offline`/`cargo test --offline` invocation dispatched into a Codex worktree must add `--no-default-features` — the sandbox has neither network nor `node`/`npm`, so `build.rs`'s shellout fails fast otherwise.

Add to "Gotchas already hit once — don't re-derive":
> Per-source-IP login rate limiting needs `ConnectInfo<SocketAddr>`, which requires `axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())` at **both** `axum::serve` call sites (`src/main.rs` and `tests/common::spawn_app`) — missing one silently compiles but the other call site never sees real client IPs. Same shape of trap as the `:id`-vs-`{id}` axum 0.7 gotcha.
>
> Cargo build scripts (`build.rs`) do not see feature activation via `cfg!(feature = "...")` — use the `CARGO_FEATURE_<NAME>` env var instead; `cfg!()` reflects `build.rs`'s own compilation, not the crate being built.

- [ ] **Step 5:** `git add CLAUDE.md && git commit -m "docs: record ui-feature Codex-sandbox gotcha and ConnectInfo dual-call-site trap"`

---

## Summary of task IDs and hard sequencing

```
A1 ─┐
A2 ─┼─ A4 (solo) ── A5 ── A6 (after A4, needs A1+A5)
A3 ─┘
                       │
B1 ── B2 (solo, after A4/A6) ── B3 ── (B4 || B5, same file) ── B6 ── B7 (parallel with B1-B6)
                                                                          │
C1 ── C2 ── (C3 || C4/C4b || C5 || C6) ── C7 (incremental)               │
                                                                          │
D1 (needs C1) ── D2 (needs A2+D1) ── D3 (needs D1+C1)                    │
                                                                          │
                    E1 (solo, last — needs all of A/B/D) ── E2 ── (E3 || E4)
```

---

## Assembly & review notes

This plan was produced by a 4-stage pipeline (Architect → Rust Engineer → 4 parallel Codex drafters → Opus review), then hand-assembled from the pipeline's raw output with the review's findings applied. Recorded here for transparency, since none of this is visible from reading the task list alone:

**Pipeline hiccup:** under concurrent load, 3 of the 4 parallel Codex subsystem-drafting agents queued as background jobs instead of returning inline (a known limitation, see `CLAUDE.md`'s Codex-sandbox notes) — the first pass of assembly briefly contained raw job-status stubs instead of content for `backend-auth`, `frontend-app`, and `build-ci-docker`. All three were recovered by polling the underlying Codex job queue directly (`codex-companion.mjs status`/`result`); one job died silently mid-run and was cancelled and re-dispatched solo. Final content for all four subsystems is real, verified TDD detail — no stubs remain in this document.

**Bugs the Opus review pass caught before implementation started (all fixed inline above, not left as follow-ups):**
- **A4/B2's file lists were incomplete** in both the architect's and engineer's drafts — 3 of the 7 real `AppState { .. }` construction sites (`tests/health_stats.rs`, `tests/admin_pools.rs`, `src/providers/refresh_lock.rs`) were missing, which would have broken `cargo test --offline` immediately once A4 landed. Fixed: both tasks' Files lists and Step 3 instructions now name all 7 sites (verified via `grep -rln "AppState {" src tests`).
- **B7 had two independently-drafted, incompatible versions** in the same assembled document — different field names (`shared_secret_origin` vs `secret_origin`), different response shapes (`{shared_secret}` only vs `{shared_secret, masked, origin}`), different file locations (flat `src/admin.rs` vs `src/admin/settings.rs`). Fixed: the richer version (masked/origin/reveal, correctly aware of A3's module-directory conversion) is canonical; the field name `secret_origin` is now used consistently across A4/B2/B3/B4/B7; C6's frontend type was updated to match the real response shape.
- **B2's login lockout had an off-by-one bug**: the original guard (`entry.failures > FAILURE_THRESHOLD`) only locked out starting on the *7th* failed attempt, while the spec and the task's own test both intend lockout after the *5th* failure (blocking the 6th attempt). Fixed: guard changed to `entry.failures >= FAILURE_THRESHOLD`.

**Known residual gap (Minor, not blocking):** the original review noted B1's test list doesn't include one specific combined scenario the spec's Testing plan section calls for — repeated renewal followed by rejection once the absolute lifetime cap is reached (the cap-enforcement logic and plain-expiry-rejection are each tested separately, just not chained together in one test). Low risk since the underlying logic is otherwise fully covered; worth adding when B1 is implemented, not worth blocking this plan's finalization over.

---
