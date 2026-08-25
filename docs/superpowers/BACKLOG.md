# 1router Backlog

Unplanned, non-urgent work that isn't tied to an active phase. Each entry
should be small enough to become a spec/plan on its own when picked up, and
should note why it matters (the incident that surfaced it, or the pain it
removes). Entries are roughly ordered by impact; no dates promised.

---

## BL-01: Misconfigured runtime flag is a permanent, silent circuit breaker

**Where:** `src/core/runtime.rs` (`ProviderRuntimeState::is_available`,
`mark_misconfigured`), `src/proxy/flow.rs`, `src/providers/refresh_task.rs`.

**What:** A provider flagged `Misconfigured` is skipped by the proxy path
**forever** — `is_available()` returns `false` unconditionally, with no
expiry and no auto-recovery. The only clears are manual admin actions
(`validate-model`, provider update, OAuth callback) or a daemon restart.

**Why it matters (incident, 2026-08-21):** a single upstream Cloudflare 403
(`error code: 1010`, transient signature/UA block on `api.commandcode.ai`)
classified as NonRetryable and permanently bricked the `command-code`
provider for the daemon's entire lifetime. Every subsequent request failed
in ~1ms with the misleading generic `"no provider produced a response"`
instead of the actual upstream error, because `handle_proxy` skips
misconfigured providers *before* building a request. Took an admin
`validate-model` poke to clear, and this ratchet re-tightens every time the
upstream briefly 403s — which is why it recurred.

**Proposed fix (design sketch):**
- Give `Misconfigured` a recovery path: store `misconfigured_since:
  Option<Instant>` and let `is_available()` return `true` again after a
  backoff window (e.g. 5–15 min), so the next request re-probes upstream.
  `record_success()` already resets the state, so a recovered upstream
  becomes self-healing.
- Optionally re-probe on a background timer instead of lazily on the next
  request.
- Surface the real upstream error body in the 503 / admin status instead of
  the generic `"no provider produced a response"` while a provider is
  skipped, so a bricked provider is diagnosable without digging.
- Keep `mark_misconfigured` as the label for genuinely dead config (e.g.
  `invalid_grant`, no refresh token), but let transient 4xx/403s take the
  timed path rather than the permanent one.

**Acceptance sketch:** after a misconfigure-triggering upstream 403, a
request succeeds again once the backoff window elapses without manual
intervention; admin status shows the real upstream error while skipped.

---

## BL-02: Split credentials out of `providers` (Option C)

**Where:** `src/core/model.rs` (`Provider`), `migrations/0001_init.sql`
(`provider_oauth_state`), both OAuth adapters
(`src/providers/adapter/codex/`, `src/providers/adapter/commandcode/`),
onboarding wizard, Providers admin UI, export/import.

**What:** A `Provider` row bundles one credential with exactly one
`upstream_model`. Serving several models from one OAuth account (e.g.
Command Code) currently means either creating several provider rows that
each hold an independent copy of the same credential, or (after
`2026-08-24-pool-member-model-identity-implementation.md`) attaching
several `model_override`d pool memberships to one provider row.

**Why it matters:** `provider_oauth_state` is `PRIMARY KEY provider_id`,
and Command Code's token refresh rotates the refresh token on every use
(`src/providers/adapter/codex/refresh.rs`). Multiple provider rows sharing
one real account run independent refresh chains against the same upstream
credential — whichever refreshes last silently invalidates its siblings'
stored tokens. This was flagged during design review for the
pool-member-model-identity work as an *active* bug in the workaround that
plan's fix replaces, not merely awkward UX.

**Proposed fix (design sketch):** a `credentials` table holding what
`provider_oauth_state` holds today, plus `providers.credential_id`, so N
model-pinned providers can share one OAuth chain safely — one refresh
chain per credential, independent of how many providers/models reference
it. Logged as a follow-up rather than folded into the pool-member-identity
plan because it touches `providers`, `provider_oauth_state`, both OAuth
adapters, the onboarding wizard, the Providers UI, and export/import — an
order of magnitude larger and orthogonal to that plan's schema fix, which
is forward-compatible with this: if credentials are split out later,
`pool_members.model_override` simply becomes unused and can be dropped.

---

## BL-03: `import_config` silently drops a pool's `strategy`/`sticky_limit`

**Where:** `src/admin/mod.rs` (`import_config`'s pool upsert).

**What:** The import path inserts pools as `(id, wire_format,
created_at)` and its `ON CONFLICT` only updates `wire_format` — it never
writes `strategy` or `sticky_limit`, even though `ExportDump.pools:
Vec<Pool>` (and the export path) does carry both (added in
`migrations/0004_pool_strategy.sql`). Exporting a `round_robin` pool and
re-importing it resurrects it as `priority` with a lost `sticky_limit`.

**Why it matters:** silent data loss on the standard backup/restore path
for any pool using the round-robin strategy work.

**Found:** during design review for
`2026-08-24-pool-member-model-identity-implementation.md` (not caused by,
or fixed by, that plan — it predates it, from the round-robin work).

**Proposed fix:** add `strategy` and `sticky_limit` to the import
statement's column list and `ON CONFLICT ... DO UPDATE SET`, matching what
the export side already serializes.

---

## BL-04: Round-robin has no per-client stickiness, so it can cause prompt-cache misses

**Where:** `src/core/state.rs` (`PoolRotationMap`, `RotationState`),
`src/pools/select.rs` (`rotate_from_cursor`).

**What:** `PoolRotationMap` is `DashMap<pool_id, RotationState>` — one
rotation cursor **per pool**, shared by every caller. There is no
client/session/API-key dimension anywhere in the key or in
`rotate_from_cursor`. `sticky_limit` counts `consecutive_uses` against the
pool globally (any caller's selection increments it), not per client. This
faithfully mirrors 9router's `combo.js` (confirmed: 9router itself has no
per-client keying either — `docs/superpowers/plans/2026-08-21-pool-round-robin-strategy-implementation.md`'s
design-reference note describes the same pool-scoped global cursor), so
this isn't a regression from porting it — the gap exists upstream too and
was never addressed by either project.

**Why it matters:** many providers/model backends give a discount or
latency win on a KV/prompt-cache hit when the *same* client keeps hitting
the *same* upstream model across a session (repeated system prompt, long
running context, etc.). With a pool-global cursor, two unrelated clients
calling milliseconds apart can each land on a different model if a
rotation boundary falls between them, and a single client's own sequential
calls have no protection once other clients' traffic is interleaved -
`sticky_limit` only slows the *global* rotation cadence, it doesn't pin any
one client. Round-robin's current design can silently defeat prompt caching
for any client whose calls straddle someone else's rotation.

**Found:** during a live e2e test of round-robin against the real gateway
(multi-provider, multi-model pool), when asked whether the mechanism
accounts for prompt-cache locality. Confirmed via code read and a targeted
Explore of 9router's design docs/comments - no mention of prompt-cache,
cache hit/miss, or per-client affinity anywhere in `src/`,
`docs/superpowers/specs/`, or `docs/superpowers/plans/` in connection with
round-robin/pools.

**Proposed fix (design sketch):** key rotation state (or a separate
affinity map) by something identifying the caller - API key, or a
client-supplied session/thread header - and change the advance trigger from
"every `sticky_limit`-th selection, pool-wide" to "advance *this client's*
pointer only on failure/exhaustion of its currently-pinned member," falling
back to the existing global cursor for callers with no identifying key.
Needs a design decision on: what identifies a "client" (bearer key already
exists and is a natural default), a bound on how many per-client cursors to
retain (unbounded `DashMap` growth under many distinct callers), and how
this interacts with `sticky_limit` (probably subsumed - per-client pinning
makes the global sticky-limit counter moot for keyed callers).

**Acceptance sketch:** with a client identity attached, N consecutive
requests from the same client return the same pool member even while other
clients' requests are being round-robined in between; a member is only
rotated away from a client after that member fails for that client (or is
marked unavailable), not on an unrelated global counter.

---

## BL-05: Command Code adapter silently drops every client sampling param (temperature fixed; logprobs confirmed unavailable)

**Where:** `src/providers/adapter/commandcode/transform.rs`
(`transform_request`).

**What:** `transform_request` builds `params` as a closed `json!({...})`
literal reading only `model`, `messages`, `max_tokens`, `tools` off the
client request. Everything else a client sends is dropped by construction:
`top_p`, `n`, `seed`, `stop`, `response_format`, `presence_penalty`,
`frequency_penalty`, `logit_bias`, `stream_options`,
`parallel_tool_calls`, `logprobs`/`top_logprobs` — **and the client's
`temperature`, which is overridden with a hardcoded `0.3`** (line 207).
No warning, no header, no log line; the request just behaves differently
than asked. Compare `adapter/codex/transform.rs`, which uses an explicit
`DISALLOWED` denylist with a comment documenting the allowlist-vs-denylist
tradeoff — Command Code has the stricter behavior and none of the
documentation.

**Why it matters:** a client pinning `temperature: 0` for
determinism/evals used to get 0.3 with no indication — **fixed 2026-08-24**
(`transform_request` now forwards the client's `temperature` when present,
falling back to 0.3 only when absent; see `transform_request_honors_the_
clients_temperature` / `..._defaults_temperature_when_the_client_omits_it`
tests). The rest — `top_p`, `n`, `seed`, `stop`, `response_format`,
`presence_penalty`, `frequency_penalty`, `logit_bias`, `stream_options`,
`parallel_tool_calls` — is still dropped by the same closed allowlist and
remains a documentation/observability gap, not yet fixed.

**`logprobs` — investigated 2026-08-24, confirmed unavailable via either
Command Code path:**
- The `OauthCommandCode` adapter's upstream, `/alpha/generate`, is a
  proprietary CLI-harness envelope (fixed field allowlist on the request,
  fixed NDJSON event vocabulary — `text-delta`, `reasoning-*`, `tool-call`,
  `tool-result`, `finish`, `error` — on the response) with structurally no
  slot for per-token probabilities. Not a bug to fix; there's nowhere to
  put the data.
- Command Code separately runs a **Provider API**
  (`https://api.commandcode.ai/provider/v1/chat/completions` /
  `/messages` — real OpenAI/Anthropic wire format, documented at
  commandcode.ai/docs/provider, gated behind a distinct paid plan tier from
  the CLI plan) that could in principle be wired up as a plain 1router
  `Passthrough` provider with zero adapter code, since same-wire-format
  passthrough already forwards `logprobs`/`top_logprobs` verbatim. **Tested
  live** against the real gateway's existing `command-code` OAuth
  credential (which the Provider API accepted as a Bearer token — 403
  `MODEL_NOT_IN_PLAN` on GPT/Gemini/Claude-tier models, not an auth
  failure) with `logprobs: true, top_logprobs: 3` against every model the
  current plan allows (`deepseek/deepseek-v4-flash`, `Qwen/Qwen3.7-Flash`,
  `moonshotai/Kimi-K2.5`): every response came back `"logprobs": null` —
  the field exists in the schema but is never populated for these
  backends/this plan tier. GPT/Gemini/Claude models (the ones most likely
  to genuinely support logprobs) are Pro-plan-gated and untested — revisit
  only if a Pro-plan Command Code credential becomes available.

**Found:** reviewing whether a logprobs-consuming client (e.g.
[llm-as-a-verifier](https://github.com/llm-as-a-verifier/llm-as-a-verifier),
which scores candidates from the logprob distribution over a score token)
could be routed through a Command Code pool member (2026-08-24).

**Remaining work:** (1) forward the params upstream plausibly accepts
(`top_p`, `stop`, `seed`) through `/alpha/generate` and confirm each
against the live API one at a time; (2) replace the implicit allowlist
with an explicit constant + comment mirroring `codex::DISALLOWED`, and
emit a debug log naming dropped fields so this is diagnosable from the
outside. `logprobs` itself needs no further work absent a Pro-plan
Command Code credential to re-test the Provider API path.

**Acceptance sketch:** a request carrying unsupported fields logs which
ones were dropped; the design doc states which client params Command Code
honors and which it cannot.
