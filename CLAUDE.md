# 1router

A lean Rust rewrite of an LLM API gateway (config-only OpenAI/Anthropic-compatible
passthrough providers, plus Codex OAuth and Command Code adapters).

**Status: all four planned workstreams are complete** (core gateway, admin UI,
onboarding wizard, release/publishing) — every task in every plan below has a
corresponding commit on `master`. There is no active implementation branch;
new work should branch directly off `master`. See:

- Core gateway — design: `docs/superpowers/specs/2026-07-25-1router-design.md`,
  plan (39 tasks, Phase 0-4, done): `docs/superpowers/plans/2026-07-25-1router-implementation.md`
- Admin UI — design: `docs/superpowers/specs/2026-07-26-admin-ui-design.md`,
  plan (28 tasks, done): `docs/superpowers/plans/2026-07-26-admin-ui-implementation.md`
- Onboarding wizard — design: `docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md`,
  plan (9 tasks, done): `docs/superpowers/plans/2026-07-26-onboarding-wizard-implementation.md`,
  smoke checklist: `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
- Command Code provider — design: `docs/superpowers/specs/2026-08-04-commandcode-provider-design.md`,
  plan: `docs/superpowers/plans/2026-08-04-commandcode-provider-implementation.md`
- Universal passthrough wire-format translation (v0.3.2) — design:
  `docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`,
  plan (done): `docs/superpowers/plans/2026-08-04-universal-passthrough-translation-implementation.md`
- Gemini provider — a "Gemini (OpenAI-compatible)" `PROVIDER_TEMPLATES`
  preset (`src/onboarding.rs` + mirrored in
  `frontend/src/pages/Providers.tsx`) was added instead, pointing at
  Google's own OpenAI-compatible endpoint
  (`https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`)
  as an ordinary `passthrough` provider — no new `ProviderKind`, no
  translation code needed. The bespoke native-adapter plan below (Gemini's
  own `generateContent`/`streamGenerateContent` wire format) was scoped but
  not pursued; design: `docs/superpowers/specs/2026-08-05-gemini-provider-design.md`,
  plan: `docs/superpowers/plans/2026-08-05-gemini-provider-implementation.md`
  (kept for reference if native-format features like `thinkingConfig` or
  exact `functionCall` shapes are ever needed)
- Release/publishing — design: `docs/superpowers/specs/2026-07-26-release-publishing-design.md`,
  plan (done): `docs/superpowers/plans/2026-07-26-release-publishing.md`,
  GHCR checklist: `docs/superpowers/plans/2026-07-26-release-publishing-ghcr-checklist.md`
  (one manual step — confirming the GHCR package is public/linked — is
  unverified from the repo alone; check via browser or a `packages:read`-scoped token)
- Progress ledger (historical, git-ignored scratch, may not exist in a fresh
  checkout): `.superpowers/sdd/progress.md`

## Build & test

```
cargo build --offline
cargo test --offline              # 30+ lib tests + integration test files
cargo test --offline --lib <path> # single module
cargo test --offline --test <name> # single integration test file
```

Package name is `router` (Cargo package names can't start with a digit); the
binary target is `1router`; the lib target is also `router`. Everything
imports via `use router::...` — never `mod core;`/`mod app;` locally in
`main.rs`, that duplicates the module tree between the bin and lib targets.

**axum is pinned to 0.7** (matchit 0.7) — dynamic path params are `:id`, NOT
`{id}` (that's axum 0.8 syntax). Getting this wrong silently 404s the route
instead of failing to compile — this bit Phase 1 for real (see plan's P1-3
fix commit). Grep for `{[a-zA-Z_]*}` in route strings if something 404s
unexpectedly.

## Onboarding

- `src/onboarding.rs` is the interactive wizard (`1router setup`, plus the
  first-boot auto-trigger). It is a thin `dialoguer` front end over
  `providers::queries` / `pools::queries` / `codex::oauth` — put no business
  logic in it. Its prompt paths are verified by
  `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`, not by
  `cargo test`. Design spec:
  `docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md`.
- The wizard also supports Command Code browser login with a paste-key
  fallback; the admin UI deliberately exposes paste-key only.
- `dialoguer` is a dependency as of Phase 5 — run `cargo fetch` with real
  network before any `--offline` work if your registry predates it.

## Working tree

`impl/v1` was the implementation branch used while the four plans above were
in progress; it has since been fully merged into `master` (it's an ancestor
of `master`'s current tip), and subsequent work (admin UI, onboarding,
releases) landed directly on `master`. Treat `master` as the active branch —
there's no standing implementation branch to branch off of anymore. For new
work, branch off `master` directly; the worktree-per-task pattern below is
historical context for how the four plans got built, not a standing
convention to keep following by default.

## Orchestration pattern (historical — how the four plans above got built)

Tasks are dispatched to the `codex:codex-rescue` subagent (Agent tool,
`subagent_type: "codex:codex-rescue"`), which forwards to OpenAI's Codex CLI
via `node "<plugin-cache>/openai-codex/codex/<version>/scripts/codex-companion.mjs" task --cwd <worktree> --write --prompt-file PROMPT.md --json`.
Each worktree gets a `TASK_BRIEF.md` (task text sliced from the plan, via
`.superpowers/sdd/task-brief.sh <plan> <task-id> <worktree>/TASK_BRIEF.md`)
and a `PROMPT.md` (fixed instructions: read the brief, use `--offline` for
cargo, don't run git commands, report DONE/DONE_WITH_CONCERNS/BLOCKED).

**Known Codex sandbox limitations** (not code bugs when these show up):
- **No network** — `cargo build`/`test` must use `--offline` against an
  already-fetched `~/.cargo/registry` + committed `Cargo.lock`. Run
  `cargo fetch` (with real network, outside Codex) whenever a task adds a
  new dependency, *including* transitive deps of `[dev-dependencies]` (a
  plain `cargo build` doesn't pull those in).
- Once the `ui` feature is default-on, every `cargo build --offline` /
  `cargo test --offline` invocation dispatched into a Codex worktree must add
  `--no-default-features` — the sandbox has neither network nor `node` /
  `npm`, so `build.rs`'s shellout fails fast otherwise.
- **Cannot bind a local TCP listener** (`TcpListener::bind` →
  `PermissionDenied`) — any test using `tests/common::spawn_app` (which
  binds a real socket for true end-to-end HTTP testing) will report BLOCKED
  or FAILED in Codex's sandbox even when the code is correct. Re-run those
  specific tests yourself outside the sandbox to verify.
- `providers::adapter::commandcode::browser_login` tests also bind a local
  listener and need the same outside-sandbox verification.
- Under concurrent load, `codex-companion.mjs task` sometimes queues as a
  background job instead of blocking — poll `node .../codex-companion.mjs
  status --cwd <worktree> --json` until `running` is empty, then `result`
  (not `--background` flag needed on your end, it self-selects).
- If a dispatch fails immediately with `"failed to load configuration"`
  (exit 1, no real work done), this is stale session state tied to that
  specific worktree *path* in the shared Codex runtime (seen after a
  worktree was removed and recreated at the same path, once the runtime's
  broker socket had rotated). Don't retry in place — recreate the worktree
  under a new directory name (e.g. `P3-7` → `P3-7b`) and dispatch there;
  it resolves immediately.

**After each task**: copy the worktree's changed files into the main
checkout, hand-merge any shared file another parallel task also touched
(`src/lib.rs` module declarations, `src/app.rs` router wiring,
`telemetry/mod.rs`/`pools/mod.rs`/`providers/mod.rs` — these collide
constantly since Phase 1 tasks run in disjoint worktrees but converge on
the same few files), run the real test suite, commit, then remove the
worktree and its branch.

**After each phase**: dispatch an Opus review agent with a generated diff
package (`git log`/`git diff --stat`/`git diff -U10` between the phase's
start and end commit, written to `.superpowers/sdd/phaseN-review.diff`) plus
the spec and plan paths. Fix Critical/Important findings before starting the
next phase; log Minor findings in the ledger.

## Gotchas already hit once — don't re-derive

- Plan originally put the `[lib]` Cargo target in a much later task (P1-1);
  it's actually needed from P0-3 onward (`cargo test --lib` requires it).
  Already fixed in the plan and code — if resuming from scratch, the lib
  target exists from P0-3.
- Any test that sets/removes process env vars (`std::env::set_var`) needs a
  `static Mutex` guard (`core::config`'s tests are the reference) — Rust
  runs `#[test]` fns on multiple threads by default and env is
  process-global. Prefer constructing `Config` directly in test harnesses
  instead of going through `Config::from_env()` + env vars at all
  (`tests/common::spawn_app` does this).
- `PassthroughAdapter::build_request` sets the auth header **by
  `wire_format`**: OpenAi → `Authorization: Bearer`, Anthropic →
  `x-api-key` + `anthropic-version: 2023-06-01`. Don't default to Bearer
  for everything.
- Per-source-IP login rate limiting needs `ConnectInfo<SocketAddr>`, which
  requires
  `axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())`
  at **both** `axum::serve` call sites (`src/main.rs` and
  `tests/common::spawn_app`) — missing one silently compiles but the other
  call site never sees real client IPs. Same shape of trap as the
  `:id`-vs-`{id}` axum 0.7 gotcha.
- Cargo build scripts (`build.rs`) do not see feature activation via
  `cfg!(feature = "...")` — use the `CARGO_FEATURE_<NAME>` env var instead;
  `cfg!()` reflects `build.rs`'s own compilation, not the crate being built.
- The admin UI's CSRF protection (`require_csrf_header` / the CSRF check
  inside `require_admin_session`) deliberately does NOT apply to
  Bearer-authenticated `/admin/*` requests — only to
  session-cookie-authenticated ones (plus the login endpoint itself, which
  has no Bearer path). This is intentional: a browser never automatically
  attaches an `Authorization` header cross-site the way it does cookies, so
  Bearer-authenticated requests are not CSRF-vulnerable in the first place.
  An early implementation applied the CSRF header requirement universally
  (all non-GET `/admin/*` regardless of auth method), which silently broke 6
  pre-existing integration tests doing admin mutations via Bearer auth
  (`admin_pools`, `admin_settings`, `admin_export_import`, `admin_providers`,
  `codex_oauth`, plus regression checks) before being caught during Phase E
  integration. Do not "fix" the Bearer exemption back to a universal CSRF
  check — that reintroduces the regression.
