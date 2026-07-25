# 1router

A lean Rust rewrite of an LLM API gateway (config-only OpenAI/Anthropic-compatible
passthrough providers, plus a Codex OAuth adapter). See:

- Design spec: `docs/superpowers/specs/2026-07-25-1router-design.md`
- Implementation plan (39 tasks, Phase 0-4): `docs/superpowers/plans/2026-07-25-1router-implementation.md`
- Progress ledger: `.superpowers/sdd/progress.md` (git-ignored scratch — check this first to see what's done)

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

## Working tree

Implementation happens on branch `impl/v1`, off `master` (which holds only
the design docs). Tasks are executed one-per-git-worktree under
`../1router-worktrees/<task-id>/` so parallel tasks can't conflict on disk;
each worktree branches from `impl/v1`'s current tip, gets its own
`impl/v1-<task-id>` branch. After a task's work is verified, it's merged
back into `impl/v1` and the worktree/branch are removed.

## Orchestration pattern (if resuming this work)

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
- **Cannot bind a local TCP listener** (`TcpListener::bind` →
  `PermissionDenied`) — any test using `tests/common::spawn_app` (which
  binds a real socket for true end-to-end HTTP testing) will report BLOCKED
  or FAILED in Codex's sandbox even when the code is correct. Re-run those
  specific tests yourself outside the sandbox to verify.
- Under concurrent load, `codex-companion.mjs task` sometimes queues as a
  background job instead of blocking — poll `node .../codex-companion.mjs
  status --cwd <worktree> --json` until `running` is empty, then `result`
  (not `--background` flag needed on your end, it self-selects).

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
