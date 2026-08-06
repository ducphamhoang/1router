# Open Access Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A hot-swappable "open access" toggle that lets `/v1/*` requests
skip the shared-secret check, controllable via `1router setup` and the
admin UI, defaulting to open on any install that has never decided this
before (interactive or headless) while never changing behavior for an
install that already had a shared secret before this feature shipped.
`/admin/*` auth is completely unaffected.

**Architecture:** New `core::settings` module (get/set a bool in the
already-existing-but-unused `server_secrets` table — **no migration**);
one new `AppState` field (`require_shared_secret: Arc<AtomicBool>` +
`auth_mode_origin`); one new branch in `auth::middleware::require_bearer`;
one new admin route pair (`admin/settings.rs`); `1router setup` restructured
into a top-level menu (Providers / Pools / Settings / Connection details /
Quit) with the toggle living in Settings; matching admin UI section +
banner escalation.

Design spec: `docs/superpowers/specs/2026-08-06-open-access-mode-design.md`
— read it before Task 1, it has the exact default-resolution rule and
wording for every prompt/banner.

**Tech Stack:** Existing deps only. No new crate, no new migration.

## Global Constraints

- Package `router`, binary `1router`; import via `use router::...`. Build/test
  with `cargo build --offline` / `cargo test --offline`.
- **No new migration.** `server_secrets(name TEXT PRIMARY KEY, value TEXT NOT NULL)`
  already exists (`migrations/0002_admin_ui.sql`) and is currently unused —
  reuse it with `name = 'require_shared_secret'`.
- Scope is `require_bearer` only. Do **not** touch
  `require_admin_session` — that's the one file where "just widen the
  same check" is the wrong move; ship the test that catches it (Task 3).
- Env var is `ROUTER_REQUIRE_SHARED_SECRET` (positive polarity — not
  `ROUTER_NO_SHARED_SECRET`). Accepts `true|false|1|0|yes|no`
  case-insensitive; anything else is a boot-time `Err`, not a silent default.
- Default-resolution rule is asymmetric on purpose — re-read the spec's
  "Default resolution" section before Task 2. It is NOT "always default
  false". It is: fresh install (`SecretSource::BootstrapNeeded` this boot) →
  default `false`; secret already existed (`Env`/`SidecarFile`) → default
  `true`. Getting this backwards silently strips auth from every existing
  deployment on upgrade.
- Every literal `AppState { .. }` construction needs the new field(s).
  Known sites: `tests/common::spawn_app`,
  `src/auth/middleware.rs::require_admin_session_tests::state()`, `main.rs`.
- axum is pinned to 0.7: any new route uses `:id`, never `{id}`.
- Tests using `tests/common::spawn_app` (real `TcpListener::bind`) are
  BLOCKED in a Codex sandbox — verify those outside it.
- With the `ui` feature default-on, any `cargo build/test --offline`
  dispatched into a Codex worktree needs `--no-default-features`.
- Out of scope (v1): rate limiting for open `/v1/*`, any `/admin/*` auth
  change, honoring this setting in export/import (explicitly excluded, not
  silently ignored — see Task 6).

---

### Task 1: `core::settings` — bool get/set on `server_secrets`

**Files:**
- Create: `src/core/settings.rs`
- Modify: `src/lib.rs` (module declaration) or `src/core/mod.rs`, whichever
  currently declares sibling modules like `core::config`

**Interfaces:**
- Produces: `pub async fn get_bool(db: &SqlitePool, name: &str) -> anyhow::Result<Option<bool>>`,
  `pub async fn set_bool(db: &SqlitePool, name: &str, value: bool) -> anyhow::Result<()>`.
  `get_bool` returns `Ok(None)` when no row exists; `Err` if the stored
  value is neither `"true"` nor `"false"` (never guess). `set_bool` is an
  upsert (`INSERT ... ON CONFLICT(name) DO UPDATE SET value = ?`).
- Consumed by: Task 2 (`main.rs`), Task 4 (admin routes).

- [ ] **Step 1: Write failing tests** — `get_bool` on an empty table
      returns `None`; `set_bool` then `get_bool` round-trips both `true`
      and `false`; a hand-inserted garbage value (`"maybe"`) makes
      `get_bool` return `Err`; `set_bool` twice on the same name updates
      rather than erroring on the `PRIMARY KEY` conflict.
- [ ] **Step 2: Run to verify it fails** — `cargo test --offline --lib core::settings` (module doesn't exist yet).
- [ ] **Step 3: Implement** `get_bool`/`set_bool` per the interface above.
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit.**
  ```bash
  git add src/core/settings.rs src/lib.rs
  git commit -m "feat(core): add settings::get_bool/set_bool on server_secrets

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
  ```

---

### Task 2: `ROUTER_REQUIRE_SHARED_SECRET`, `listen_addr_is_loopback`, and the default-resolution rule

**Files:**
- Modify: `src/core/config.rs` — add `AuthModeSource` enum (mirrors
  `SecretSource`: `Env(bool)`, `Db(bool)`, `Default(bool)`), a
  `parse_require_shared_secret_env() -> anyhow::Result<Option<bool>>`
  (reads `ROUTER_REQUIRE_SHARED_SECRET`, `None` if unset, `Err` on garbage),
  and `pub fn listen_addr_is_loopback(addr: &SocketAddr) -> bool`.
- Modify: `src/core/state.rs` — add
  `AuthModeOrigin { Env, Db, Default }` (mirror `SecretOrigin`'s
  `#[derive(Clone, Copy, ..., serde::Serialize)]` shape), and on
  `AppState`: `pub require_shared_secret: Arc<std::sync::atomic::AtomicBool>`,
  `pub auth_mode_origin: AuthModeOrigin`.

**Interfaces:**
- Consumes: nothing new (reads env + the `Option<bool>` Task 1 gives it).
- Produces: the pure resolution function
  `pub fn resolve_auth_mode(secret_source: &SecretSource, db_value: Option<bool>) -> anyhow::Result<AuthModeSource>`
  in `config.rs` — takes the *already-resolved* secret source (so it can
  apply the asymmetric default) and the DB row (`None` if no row yet), and
  `ROUTER_REQUIRE_SHARED_SECRET` internally. This is called from `main.rs`
  in Task 5, and is unit-testable without a DB or a running server.

- [ ] **Step 1: Write failing tests** in `core::config`:
  - `parse_require_shared_secret_env` — unset → `Ok(None)`; `"true"`/`"1"`/`"yes"` (any case) → `Ok(Some(true))`; `"false"`/`"0"`/`"no"` → `Ok(Some(false))`; `"maybe"` → `Err`.
  - `listen_addr_is_loopback` — `127.0.0.1:8080` and `[::1]:8080` → `true`; `0.0.0.0:8080` and a LAN IP → `false`.
  - `resolve_auth_mode`:
    - env set → wins regardless of `secret_source`/`db_value`, origin `Env`.
    - env unset, `db_value = Some(x)` → returns `x`, origin `Db`, regardless of `secret_source`.
    - env unset, `db_value = None`, `secret_source = BootstrapNeeded` → `false`, origin `Default`.
    - env unset, `db_value = None`, `secret_source = Env(_)` or `SidecarFile(_)` → `true`, origin `Default`.
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement** per the interface above. Use the existing
      `ENV_LOCK` mutex pattern (`core::config`'s tests already have one) for
      the env-touching tests.
- [ ] **Step 4: Run to verify it passes** — `cargo test --offline --lib core::config`.
- [ ] **Step 5: Commit.**

---

### Task 3: `require_bearer`'s new branch — and the test that proves `require_admin_session` is untouched

**Files:**
- Modify: `src/auth/middleware.rs` — `require_bearer`: if
  `!state.require_shared_secret.load(Ordering::Relaxed)`, skip straight to
  `next.run(req).await`. **Do not touch `require_admin_session`.**
- Modify: `src/auth/middleware.rs::require_admin_session_tests::state()` —
  add the two new `AppState` fields (default `require_shared_secret: true`
  so all existing tests in that module keep their current meaning
  unchanged).

**Interfaces:**
- Consumes: `AppState.require_shared_secret` (Task 2).
- Produces: nothing new consumed elsewhere; this is the enforcement point
  itself.

- [ ] **Step 1: Write failing tests**, in `auth::middleware`'s own test
      module (add one alongside the existing `csrf_tests`/
      `require_admin_session_tests`, e.g. `require_bearer_tests`):
  - `require_bearer_open_mode_allows_no_header` — `require_shared_secret = false`, no `Authorization` header → 200.
  - `require_bearer_closed_mode_still_rejects_no_header` — `require_shared_secret = true` (existing behavior) → 401, unchanged.
  - **The load-bearing one**: `require_admin_session_still_rejects_with_no_credential_when_open_access_is_on` — build the `require_admin_session_tests::app()` harness with `require_shared_secret = false`, no cookie, no bearer → still 401. This is the test that fails if a future edit widens the wrong function.
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement** the `require_bearer` branch.
- [ ] **Step 4: Run to verify it passes** — `cargo test --offline --lib auth::middleware`.
- [ ] **Step 5: Commit.**

---

### Task 4: Admin API — `GET`/`PATCH /admin/settings/auth-mode`, extend `security-status`

**Files:**
- Modify: `src/admin/settings.rs` — add the route pair to `routes()`;
  `AuthModeResponse { require_shared_secret: bool, origin: AuthModeOrigin }`;
  `get_auth_mode`/`patch_auth_mode` (409 on `AuthModeOrigin::Env`, same
  message shape as `patch_shared_secret`'s existing guard); on success,
  `settings::set_bool(&s.db, "require_shared_secret", value)` then
  `s.require_shared_secret.store(value, Ordering::Relaxed)`. Extend
  `SecurityStatusResponse` with `require_shared_secret: bool` and
  `listen_addr_is_loopback: bool` (from `s.config.listen_addr`).

**Interfaces:**
- Consumes: `settings::set_bool` (Task 1), `AppState.require_shared_secret`/`auth_mode_origin` (Task 2), `config::listen_addr_is_loopback` (Task 2).
- Produces: the two endpoints Task 8 (frontend) calls.

- [ ] **Step 1: Write failing tests** in `tests/admin_settings.rs` (mirror the existing shared-secret tests):
  - `auth_mode_get_reflects_current_state_and_origin`.
  - `auth_mode_patch_toggles_and_takes_effect_on_next_request_without_restart` — PATCH to open, then a `/v1/*` request with no `Authorization` succeeds immediately (same process, no restart) — this is the one that actually exercises the `AtomicBool` hot-swap end to end.
  - `auth_mode_patch_conflicts_when_env_controlled` — set `ROUTER_REQUIRE_SHARED_SECRET`, PATCH → 409.
  - `security_status_reports_require_shared_secret_and_loopback`.
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify it passes** — `cargo test --offline --test admin_settings` (needs `tests/common::spawn_app`; verify outside a sandboxed environment per the global constraints).
- [ ] **Step 5: Commit.**

---

### Task 5: Wire it into `main.rs` boot + the non-loopback warning

**Files:**
- Modify: `src/main.rs` — after `init_pool`, call
  `config::resolve_auth_mode(&resolved_secret, settings::get_bool(&db, "require_shared_secret").await?)`;
  if the row didn't exist yet (`AuthModeSource::Default(_)`), persist it via
  `settings::set_bool` so the default is only computed once, ever, per the
  design. Construct `AppState` with the resolved value/origin. Add the
  boot warning (WARN, every boot) when `!require && !listen_addr_is_loopback`,
  exact wording from the design spec.
- Modify: `tests/common::spawn_app` — add the new `AppState` fields
  (`require_shared_secret: true` default, so existing integration tests are
  unaffected unless a test opts into open mode explicitly). Consider adding
  an optional builder/param if several upcoming tests need open mode — check
  Task 4's and Task 7's test needs before deciding the exact signature.

- [ ] **Step 1: Write failing tests** — an integration-style test (likely
      in `tests/admin_settings.rs` or a new `tests/open_access.rs`):
  - **Upgrade regression** (the most important test in this whole plan):
    boot with an existing `.router_secret` file and no `server_secrets`
    row → `/v1/chat/completions` with no `Authorization` → 401.
  - **Fresh-bootstrap default**: boot with no secret file, no env secret, no
    `server_secrets` row (simulating first-ever boot) → resolved
    `require_shared_secret` is `false`.
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit.**

---

### Task 6: Exclude the setting from export/import

**Files:**
- Modify: wherever `/admin/export` / `/admin/import` currently serialize
  config (locate via `grep -n "export\|import" src/admin/*.rs`) — confirm
  the auth-mode setting is not part of that struct at all (it shouldn't be,
  since it's not stored alongside providers/pools) and add a one-line
  comment recording that the exclusion is deliberate, plus a test that
  round-trips export→import and asserts `require_shared_secret` is
  unchanged by the import regardless of what (if anything) the imported
  JSON contains under that key.

- [ ] **Step 1: Write failing test** (or, if the struct genuinely has no
      such field today, write the "import doesn't affect it" test directly
      — it may pass immediately, which is fine, it's still a regression
      guard).
- [ ] **Step 2: Run to verify current behavior.**
- [ ] **Step 3: Add the guard comment / any needed exclusion.**
- [ ] **Step 4: Run to verify it passes.**
- [ ] **Step 5: Commit.**

---

### Task 7: `1router setup` menu restructure + the access-mode prompt

**Files:**
- Modify: `src/onboarding.rs` — split `run_wizard` into:
  - `run_first_boot_wizard(db, http, sqlite_path)` — today's linear flow,
    with the access-mode prompt inserted right after
    `resolve_or_prompt_secret` (only reachable via the `BootstrapNeeded`
    path — mirror the design spec's loopback/non-loopback prompt text
    exactly, including the typed-`OPEN` confirmation for non-loopback).
  - `run_menu(db, http, sqlite_path)` — new top-level `Select` loop
    (Providers / Pools / Settings / Connection details / Quit) per the
    design spec's mockup. Providers/Pools submenus wrap the existing
    provider-adding/pool-assigning loop bodies (extract into small helper
    fns if `run_wizard`'s current body doesn't already separate them
    cleanly). Settings submenu: `/v1 access mode` (Select, same
    confirmation rules as above), `API key` (show/change, wraps
    `resolve_or_prompt_secret` plus a new rotate path calling
    `config::persist_secret` directly, mirroring what the admin PATCH does),
    `Admin UI password` (calls existing `reset_admin_password`), `Back`.
    Use `interact_opt()` everywhere in the new menus so Esc/Ctrl-C returns
    to the parent menu instead of erroring.
- Modify: `src/main.rs` — first-boot auto-trigger calls
  `run_first_boot_wizard`; the `setup` subcommand handler calls `run_menu`.

**Interfaces:**
- Consumes: `settings::set_bool`/`get_bool` (Task 1),
  `config::listen_addr_is_loopback` (Task 2), `Config.listen_addr` (for the
  menu header's "listening on ..." line).
- Produces: nothing consumed by other Rust code — this is a leaf.

- [ ] **Step 1: Write failing tests** (onboarding.rs's existing test module
      already unit-tests helper logic without a TTY — cannot test
      `dialoguer` prompts directly, but test the extractable pure pieces:
      the menu header line's formatting given a provider/pool count and
      access-mode string, and any new pure helper functions). Note in the
      commit/PR that the interactive paths themselves are covered by
      `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`-style
      manual smoke testing, not `cargo test` — update that smoke checklist
      with the new menu's steps as part of this task.
- [ ] **Step 2: Run to verify it fails / passes for the pure pieces.**
- [ ] **Step 3: Implement the restructure.**
- [ ] **Step 4: `cargo build --offline` and `cargo test --offline --lib onboarding`; manually smoke-test the new menu interactively (TTY required — cannot be done by an agent in a non-interactive sandbox; flag as a human/outside-sandbox verification step).**
- [ ] **Step 5: Commit.**

---

### Task 8: Admin UI — `Settings.tsx` "Client API access" section + `App.tsx` banner escalation

**Files:**
- Modify: `frontend/src/pages/Settings.tsx` — new "Client API access"
  section (radio pair, confirm-on-non-loopback flow) inserted between the
  password form and the shared-secret form, per the design spec's mockup;
  disable+annotate (not hide) the shared-secret form when open; adjust the
  "Connect a client" `curl` snippet to drop the `Authorization` header line
  when open.
- Modify: `frontend/src/App.tsx` — `SecurityStatus` type grows
  `require_shared_secret`/`listen_addr_is_loopback`; `SecurityBanner` adds
  the open-access message (both severities per the design spec).
- Modify: `frontend/src/pages/Providers.form.test.tsx` or add a new
  `Settings.test.tsx` if one doesn't exist — check
  `ls frontend/src/pages/*.test.tsx` first.

- [ ] **Step 1: Write failing frontend tests** — radio pair renders current
      state; selecting Open on a loopback status requires only the base
      confirm; selecting Open on non-loopback status requires the extra
      confirmation step; shared-secret form is disabled (not absent) when
      open; banner shows the escalated message when
      `!require_shared_secret && !listen_addr_is_loopback`.
- [ ] **Step 2: Run to verify it fails** — `npm test` (check `frontend/package.json` for the exact script name).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify it passes**, and `npm run build` (or equivalent) to catch type errors.
- [ ] **Step 5: Commit.**

---

### Task 9: README + CLAUDE.md notes

**Files:**
- Modify: `README.md` — document `ROUTER_REQUIRE_SHARED_SECRET`, the
  open-access mode and its default-resolution rule, the non-loopback
  warning, and the explicit non-goal that it implies no rate limiting.
- Modify: `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md` —
  add the new menu's smoke-test steps (done as part of Task 7, listed here
  as the final check that it landed).

- [ ] **Step 1: Write the README section.**
- [ ] **Step 2: Confirm the smoke checklist was updated in Task 7.**
- [ ] **Step 3: Commit.**
