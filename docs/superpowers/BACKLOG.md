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
