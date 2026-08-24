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
