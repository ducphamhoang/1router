# 1router interactive onboarding wizard — design

## Problem

1router's admin surface is API-only by design (v1 non-goal: "Built-in web
dashboard UI"). That's fine once configured, but getting from a fresh binary
to a working provider today requires knowing, in order: the
`ROUTER_SHARED_SECRET` env var is mandatory and must be set before the
process will even start; the exact JSON shapes for `POST /admin/providers`,
`POST /admin/pools`, `PUT /admin/pools/:id/members`; and, for a Codex
provider, the two-step OAuth dance (`/oauth/start` → open a URL in a browser
→ paste back `code`/`state` → `/oauth/complete`) plus the fact that the
`upstream_model` string accepted by the real backend is account/plan-specific
and not discoverable except by trying candidates against a live login.

None of that is written down anywhere a new user would find it, and none of
it can be done without either hand-crafting curl commands or writing a
one-off script. This design adds an interactive terminal wizard, run inside
the `1router` binary itself, that walks a user through exactly that setup.

## Goals

- Zero-to-configured in one guided session: shared secret, one provider
  (passthrough or Codex OAuth), one pool.
- Works both as an automatic first-boot step and as an explicit,
  re-runnable subcommand.
- No new HTTP surface, no new business logic — the wizard is a thin
  interactive front end over the same functions the admin API already
  calls (`providers::queries::*`, `pools::queries::*`,
  `providers::adapter::codex::oauth::*`).
- Never hangs a headless/scripted deployment waiting for input that will
  never come.

## Non-goals

- A web UI (still an explicit v1 non-goal; this is a terminal-only wizard).
- Automating the actual browser login step for Codex OAuth — a human must
  still open the URL and authorize (unchanged from today).
- Editing/removing existing providers or pools interactively — the wizard
  only *adds*; changes to existing config remain the admin API's job.
- A general-purpose CLI argument parser — one subcommand
  (`1router setup`) is handled with a plain `std::env::args()` check, not a
  new dependency like `clap`.

## Triggers

Two entry points share one implementation, `onboarding::run_wizard`:

1. **First boot**, inside `main()`, immediately after `init_pool` and before
   `load_snapshot`: runs the wizard automatically if all of the following
   hold:
   - the `providers` table is empty (`SELECT count(*) FROM providers` — the
     same signal `seed.rs` already uses for its own guard), **and**
   - `ROUTER_SEED_PATH` is not set (an explicit seed file means the operator
     wants scripted/automated config; it always wins over the interactive
     path and the wizard is skipped even with a TTY attached), **and**
   - stdin is a TTY (`std::io::IsTerminal::is_terminal`).
2. **Explicit subcommand**: `1router setup`. Runs the same wizard on
   demand — e.g. to add a second provider later — regardless of whether the
   DB is already populated. Requires a TTY; if stdin isn't one, it prints an
   error to stderr and exits with a non-zero status rather than blocking on
   a read that will never resolve.

If the DB is empty, no seed path is set, and there is **no** TTY (a headless
first boot — the common Docker/systemd case), the wizard is skipped
entirely and the process falls through to today's normal startup, subject
to the shared-secret bootstrap behavior below.

## Shared-secret bootstrap

Today, `Config::from_env()` requires `ROUTER_SHARED_SECRET` to already be
set and errors out immediately if it's missing — there is no other source
and no persistence. That remains the top-priority source, but this design
adds a fallback so a wizard-generated (or auto-generated) secret survives
process restarts without needing the env var re-supplied every time.

**New sidecar file**: `<directory containing sqlite_path>/.router_secret`,
created with mode `0600`.

**Resolution order**, replacing the current hard error:
1. `ROUTER_SHARED_SECRET` env var, if set — always wins, unchanged from
   today. This remains the recommended path for real deployments (e.g.
   injected by a secrets manager), since an explicit env var shouldn't be
   silently shadowed by a stale sidecar file.
2. Else, the sidecar file, if it exists and is readable — its contents
   (trimmed) are used as the secret.
3. Else, neither exists: this is the bootstrap-needed case.
   - **TTY**: the wizard's first step asks "Generate a random admin secret,
     or enter your own?" (select). A random secret is 32 bytes from a CSPRNG,
     hex-encoded. Either way, the result is written to the sidecar file
     before anything else in the wizard proceeds (the file's existence is
     also what lets a later `1router setup` invocation skip re-asking for a
     secret).
   - **No TTY**: auto-generate the same way, write the sidecar file, and log
     it once at `info` level with an explicit "save this now, it will not
     be logged again" message, then continue booting normally.

A corrupt/unreadable-but-present sidecar file (e.g. bad permissions after a
manual edit) is a fail-fast error, same posture as a missing env var today —
it does not silently fall through to generating a new one, since that would
invalidate whatever secret was previously handed out to real callers.

## Wizard flow

Once a secret is resolved (from any of the three sources above), the wizard
proceeds:

1. If `providers` count is 0 (always true on first boot; may be false when
   run via `1router setup` later): ask **"Add a provider now? (Y/n)"**. If
   no, print a one-line hint ("configure later via the admin API — see
   README") and finish.
2. **Provider kind** (select): `(1) passthrough` / `(2) Codex OAuth (ChatGPT
   account)`.
   - **passthrough**: prompt, in order: `name` (used as both `Provider.id`
     and `Provider.name`), `wire_format` (select: openai/anthropic),
     `base_url`, `api_key` (masked input), `upstream_model`. Calls
     `providers::queries::create_provider` directly with these values.
   - **Codex OAuth**: prompt only `name` (same id/name doubling). Creates
     the provider via `create_provider` with `wire_format: openai`,
     `kind: oauth_codex`, `base_url: None`, `api_key: None`,
     `upstream_model` set to a temporary placeholder (`"pending"`). Then:
     - Calls `oauth::generate_pkce` + `oauth::build_authorize_url` directly
       (no HTTP hop), prints the URL, and blocks on stdin (same parsing
       logic already proven in `tests/e2e_real_providers.rs`: accept either
       a full pasted redirect URL or a bare `code=...&state=...` fragment).
     - Exchanges the code in-process (the same function
       `oauth_routes::complete`'s handler calls), persists tokens via
       `queries::upsert_oauth_tokens`. An exchange failure (bad/expired
       code, state mismatch) is reported and the user is re-prompted to
       paste again, without restarting the whole wizard.
     - **Model probe**: once tokens are stored, send one real, minimal
       chat-completion request per candidate in
       `["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5",
       "codex-mini-latest"]` (same list as the e2e test), setting
       `upstream_model` via `queries::update_provider` before each attempt.
       Stop at the first `200`, report which model was selected, and leave
       `upstream_model` set to it. If every candidate fails, print all the
       failures (status + body) and leave `upstream_model` as `"pending"`,
       telling the user to set it manually via `PATCH /admin/providers/:id`
       once they know the right value.
3. Prompt for a **pool id** to add the new provider to. If the pool doesn't
   exist yet, create it (`wire_format` matched to the provider's). Add the
   provider as a member at priority 1 (first member in a fresh pool; if the
   pool already has members, priority defaults to `max(existing) + 1` so it
   doesn't silently jump the queue in front of an existing provider).
4. Ask **"Add another provider? (y/N)"** — loop to step 2, or finish and
   (for the first-boot path) continue into normal startup; (for `setup`)
   exit 0.

## Components touched

- **New module `src/onboarding.rs`**: the wizard itself. Built on
  `dialoguer` (new dependency — text input, password/masked input,
  select-from-list, confirm) for the prompt UI. Contains no business logic
  of its own beyond sequencing calls into existing modules
  (`providers::queries`, `pools::queries`,
  `providers::adapter::codex::oauth`, plus one real HTTP request per model
  probe attempt via the shared `reqwest::Client`).
- **`src/core/config.rs`**: add the secret-resolution fallback described
  above (env → sidecar file → bootstrap-needed signal), replacing the
  current unconditional `shared_secret` error. The bootstrap-needed case is
  surfaced as a distinct return variant so `main.rs` knows whether it still
  needs to run wizard-or-autogenerate before a `Config` can be fully built,
  since `Config` itself is otherwise immutable/complete once constructed.
- **`src/main.rs`**: wire the trigger check (empty DB + no seed path + TTY)
  before `load_snapshot`, handle the no-TTY auto-generate case, and add a
  `setup` subcommand branch checked via `std::env::args().nth(1)` at the top
  of `main`, before any other startup work.

## Error handling

- **Ctrl-C / EOF mid-wizard**: the process exits (non-zero for `setup`,
  propagated as a startup failure for the first-boot path — a wizard that's
  interrupted partway shouldn't silently continue into serving traffic with
  half-finished config the operator never confirmed). Whatever provider/pool
  rows were already committed in prior *completed* loop iterations remain
  (each iteration commits independently, matching how the admin API already
  behaves) — there is no new cross-iteration transactionality requirement.
- **Invalid OAuth code/state paste**: surfaces the same error
  `/oauth/complete` already returns (reusing its underlying function), and
  re-prompts for the paste within the same step rather than aborting the
  whole wizard or requiring the user to redo `/oauth/start`.
- **Model-probe total failure**: not treated as an error — the provider is
  left in place with `upstream_model: "pending"` and the wizard continues to
  the pool-assignment step; the user fixes the model value later via the
  admin API once they know it.
- **Sidecar secret file unreadable/corrupt**: fail-fast error at startup,
  same posture as today's missing-env-var error.

## Testing

- Unit tests for `Config`'s new secret-resolution fallback (env wins;
  sidecar file read when env unset; bootstrap-needed signal when neither
  exists), following the existing `ENV_LOCK`-guarded pattern already used
  for env-touching tests in `config.rs`.
- Unit tests for pool-priority defaulting (`max(existing) + 1`) and for the
  candidate-model-probe loop's stop-at-first-200 logic, extracted as plain
  functions the wizard calls (so they're testable without a real terminal or
  real network).
- The interactive prompt flow itself (the `dialoguer` calls) is not
  practically unit-testable and gets a documented manual smoke test instead,
  relying on the fact that every action it triggers
  (`create_provider`/`update_provider`/`upsert_oauth_tokens`/pool
  queries/OAuth exchange) is already covered by existing tests via the admin
  API and `tests/codex_oauth.rs`.
- No new integration-test infra is needed — this feature doesn't add any new
  HTTP routes.

## Open questions / accepted risk

- The candidate model list is a snapshot of what's known to work today
  (confirmed `gpt-5.4` works for at least one real ChatGPT account as of
  2026-07-26); it may drift out of date as OpenAI changes its backend
  allowlist. This is accepted as a v1 limitation, same as the existing e2e
  test's list — both should be updated together if/when they're found to be
  stale.
