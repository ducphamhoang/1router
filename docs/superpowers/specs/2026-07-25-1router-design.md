# 1router — Design Spec

## Motivation

A Rust rewrite of [decolua/9router](https://github.com/decolua/9router), a Node.js/Next.js LLM
API gateway that unifies many providers behind one endpoint. Reasons for the rewrite:

- **Lean resource footprint** — runs on tiny VPS/sidecar deployments with low idle memory/CPU.
- **Predictable tail latency** — no GC pauses; this sits in the hot path of every LLM request.
- **Single static binary** — trivial to distribute (Docker `scratch`/distroless, or bare binary).

Scope is deliberately narrower than 9router: no per-client virtual keys/billing, no combo/fusion
multi-model panels, no built-in web UI. A "provider" is, by default, a pure config row (base URL
+ credential + upstream model name), not compiled/dynamic plugin code — **with exactly one
deliberate, scoped exception**: a Codex (ChatGPT-account) adapter, since Claude Code and other
coding-agent tool use is the primary motivating use case and Codex is a popular free/subscription
backend for it. See "Codex Adapter" below — this is the only provider in v1 with OAuth, token
refresh, or request/response transformation; every other provider stays pure passthrough config.

## Wire formats supported

Two passthrough formats, each byte-for-byte, **no translation between them**:

- `POST /v1/chat/completions` → OpenAI-compatible upstreams (OpenRouter, Groq, Together.ai,
  Fireworks, DeepSeek, Mistral, Ollama/llama.cpp server, etc.)
- `POST /v1/messages` → Anthropic-compatible upstreams (needed because Claude Code speaks the
  Anthropic Messages API, not OpenAI's — several providers such as z.ai/GLM, Kimi, DeepSeek
  expose an Anthropic-compatible endpoint)

A **pool is homogeneous**: every member of a pool speaks the same wire format. The route the
client hits determines which pools are eligible.

Also required for client compatibility: `GET /v1/models` (returns pool names) — OpenAI-SDK-based
tools like Cursor populate their model picker from this; without it they show empty/error.

## Architecture

Single binary (`1router`), axum + tokio + sqlx (SQLite). Two logical HTTP surfaces, same
shared-secret bearer auth:

- **Proxy surface** `/v1/*` — the gateway itself
- **Admin surface** `/admin/*` — CRUD for providers/pools, stats, health

**Module layout — feature-first**, each module owning its full vertical slice (routes, queries,
logic). This was independently confirmed by both the architecture review and by 9router's own
codebase (`open-sse/{handlers,providers,translator,services}` each own their domain end-to-end,
with only a thin repo layer underneath):

```
src/
  proxy/      /v1 routes: pool selection, failover loop, streaming passthrough
  pools/      admin routes + selection logic + queries
  providers/  admin routes + queries + a /test connectivity endpoint (borrowed from 9router)
  auth/       shared-secret middleware
  telemetry/  request_log + stats + tracing + /health
  core/       sqlx pool + migrations, config, reqwest client, shared error types
  app.rs      Router::merge wiring
```

`proxy` depends on `pools::select()` and `providers` — one directional edge, kept intentionally
simple. Adding a future feature (e.g. virtual keys) means adding a new `keys/` module with zero
edits to existing ones.

## Data model (SQLite, via `sqlx::migrate!` from day one)

```sql
providers (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  wire_format TEXT NOT NULL,      -- client-facing format this provider satisfies: 'openai' | 'anthropic'
  kind TEXT NOT NULL DEFAULT 'passthrough',  -- 'passthrough' | 'oauth_codex'
  base_url TEXT,                  -- NULL for oauth_codex (fixed upstream, set by the adapter)
  api_key TEXT,                   -- NULL for oauth_codex (uses provider_oauth_state instead)
  upstream_model TEXT NOT NULL,
  created_at, updated_at
)

provider_oauth_state (            -- present for any oauth_* kind, not Codex-specific
  provider_id TEXT PRIMARY KEY REFERENCES providers(id),
  access_token TEXT, refresh_token TEXT, id_token TEXT,
  access_expires_at TIMESTAMP,
  provider_data TEXT NOT NULL DEFAULT '{}',  -- JSON blob: chatgpt_account_id/workspace_id for
                                              -- Codex, project_id for a future Gemini adapter, etc.
                                              -- kept generic (matches 9router's providerSpecificData
                                              -- pattern) so a second adapter needs no schema change
  pkce_verifier TEXT, oauth_state TEXT,   -- only populated mid-flow, cleared after completion
  updated_at
)

pools (
  id TEXT PRIMARY KEY,            -- logical model name clients request, e.g. "gpt-4o"
  wire_format TEXT NOT NULL,      -- must match all member providers' wire_format
  created_at
)

pool_members (
  pool_id TEXT REFERENCES pools(id),
  provider_id TEXT REFERENCES providers(id),
  priority INTEGER NOT NULL,
  PRIMARY KEY (pool_id, provider_id)
)
-- INDEX pool_members(pool_id)  -- hot failover lookup

request_log (
  id INTEGER PRIMARY KEY,
  pool_id TEXT, provider_id TEXT,
  status_code INTEGER, latency_ms INTEGER, success BOOLEAN,
  created_at
)
-- INDEX request_log(pool_id, created_at), (provider_id, created_at)
```

Normalized `pool_members` (not a JSON array, unlike 9router's `combos.models` blob) — gives FK
integrity and per-member queries, confirmed as the right call over 9router's schema pattern.

**`provider_state` (cooldown/backoff) lives in memory, not SQLite** — a `DashMap`/`RwLock<HashMap>`
keyed by `provider_id`. Concurrent failing requests doing SELECT-then-UPDATE against SQLite would
lose updates under load; in-memory state avoids the write-contention entirely. This mirrors
9router's own approach of keeping cooldown state inline rather than in a separate log table.

```rust
struct ProviderRuntimeState {
    backoff_level: u8,           // 0 = healthy
    unavailable_until: Option<Instant>,
    status: ProviderStatus,      // Healthy | Cooling | Misconfigured
}
```

**SQLite pragmas:** WAL mode + `busy_timeout`. `request_log` inserts are pushed off the request
path through a bounded channel to a single writer task with batched inserts, so logging never
serializes the hot path on SQLite's single writer.

**Config caching:** providers/pools are read into an in-memory snapshot on boot and refreshed on
every admin mutation (not re-queried per request) — avoids a DB hit per proxy request.

## Failover & backoff logic

Adopting 9router's precedent directly (`open-sse/services/accountFallback.js`,
`open-sse/config/errorConfig.js`):

- Exponential backoff: `cooldown = min(2s * 2^(level - 1), 5min)`, capped at 15 escalation levels.
- A small **rule table** matches upstream status code / error text, top to bottom:
  - **Non-retryable** (401 invalid key, 400 malformed request/config) → mark provider
    `Misconfigured` immediately (not just "cooling down") and surface it loudly via stats/logs —
    a dead key must never silently loop in backoff forever.
  - **Retryable** (429, 5xx, timeout) → bump `backoff_level`, apply exponential cooldown.
  - Provider-reported precise reset time (e.g. a `retry-after` header) overrides the computed
    cooldown, capped at 30min.
  - Unmatched errors → flat 30s cooldown.
- Explicit success clears state: `backoff_level = 0`, `status = Healthy`.

### Request flow

```
1. Auth middleware checks shared secret → 401 if missing/wrong
2. Route by path (/v1/chat/completions → openai pools, /v1/messages → anthropic pools)
3. Look up pool → ordered provider list, skip any Cooling (unavailable_until > now) or Misconfigured
4. Buffer the request body (needed to retry against the next provider on failure — a consumed
   stream can't be replayed). Cap buffer size, coordinated with the body-size limit below.
5. For each provider:
   a. Rewrite body.model → provider.upstream_model
   b. Forward via reqwest, with layered timeouts: connect timeout + response-headers (TTFB)
      timeout + inter-chunk idle timeout — deliberately NO total deadline on the streamed body,
      so long valid streams aren't killed.
   c. stream:true → reqwest .bytes_stream() → axum Body::from_stream, true passthrough with
      backpressure, never buffered in full.
   d. 2xx → log success (async, off hot path), clear provider state, return response to client
   e. Non-retryable error → mark Misconfigured, do NOT try next provider automatically for this
      class (config errors don't fix themselves by trying a different key), surface clearly
   f. Retryable error → log failure, apply backoff, try next provider in pool
6. All exhausted/unavailable → 503, with the LAST meaningful upstream error body + which
   provider produced it (not an opaque generic message), plus `x-1router-tried: A,B` and
   `x-1router-error` response headers for operator debugging
```

**Streaming commitment point:** once bytes have started flowing to the client, failover can no
longer happen — can't swap providers mid-SSE-response without producing garbled output.
**Known accepted limitation:** some providers return HTTP 200 then emit an error *inside* the SSE
body; since this is pure passthrough (no body inspection), that surfaces to the client as a
truncated response rather than a clean error. This is inherent to the no-translation design —
log it distinctly so it's diagnosable, don't try to fix it by parsing SSE bodies.

## Codex Adapter

The one deliberate exception to "provider = config only." Codex (OpenAI's ChatGPT-account-based
coding backend) requires OAuth login, proactive token refresh, and real request/response
transformation to present as a normal OpenAI-shaped provider to clients — none of which fits the
passthrough model. This was scoped in specifically because it directly serves the primary use
case (coding-agent tool use), based on precedent studied directly from 9router's implementation
(`open-sse/providers/registry/codex.js`, `open-sse/executors/codex.js`,
`open-sse/services/tokenRefresh.js`, `src/lib/oauth/services/codex.js`).

**`ProviderAdapter` trait** — the extension point. Every provider goes through this; `passthrough`
is a trivial identity implementation, `oauth_codex` is the one real one:

```rust
trait ProviderAdapter {
    async fn build_request(&self, client_body: &Body, creds: &Credentials) -> reqwest::Request;
    async fn transform_response(&self, upstream: Response, client_wanted_stream: bool) -> ClientResponse;
    fn classify_error(&self, resp: &Response) -> ErrorClass;
    // Refresh is inherently per-provider (Codex sends JSON, others form-encoded, others AWS-signed) —
    // this seam is what makes a second adapter (Gemini-CLI, Antigravity) a real drop-in later,
    // not a rework of the trait.
    fn needs_refresh(&self, creds: &Credentials) -> bool;
    async fn refresh_credentials(&self, creds: &Credentials) -> Result<Credentials, RefreshError>;
}
```

Codex's implementation, matching 9router's `CodexExecutor`:
- Targets OpenAI's Responses API (`https://chatgpt.com/backend-api/codex/responses`), not Chat
  Completions. Request transform: `system` role → `developer`, strips server-generated item IDs,
  forces `store: false` and `stream: true` upstream regardless of what the client asked for,
  injects `prompt_cache_key`/session id, defaults `reasoning.effort` and
  `include: ["reasoning.encrypted_content"]`, and — critically — **applies a strict allowlist that
  deletes any field Codex's backend rejects**: `temperature`, `top_p`, `max_tokens`,
  `max_output_tokens`, `user`, and others. Ordinary OpenAI-SDK clients (Cursor, etc.) send these
  fields normally; forwarding them unfiltered to Codex produces a 400/`routing_unsupported`. This
  allowlist is not optional polish, it's required for any real request to succeed.
- Sets identity headers `originator: codex_cli_rs` and `User-Agent: codex_cli_rs/<version>`, plus
  `ChatGPT-Account-ID` from `provider_oauth_state.provider_data`.
- **Streaming mismatch:** because Codex is forced to `stream: true` upstream even when the client
  did not request streaming, `transform_response` must aggregate the upstream SSE into a single
  JSON response when `client_wanted_stream == false`. The general passthrough path (client
  stream-flag == upstream stream-flag) does not hold for this adapter — it's handled entirely
  inside `transform_response`, not by the shared proxy request flow.
- Peeks inside 200-OK SSE bodies for embedded `usage_limit_reached`/transient-error events (Codex
  reports some failures inside the stream body, not via HTTP status) — feeds `classify_error`.

**OAuth flow — two admin calls, no local callback server required** (OpenAI's OAuth client has a
fixed registered redirect URI, `http://localhost:1455/auth/callback`, so the router can't just
redirect to itself; this works whether 1router runs locally or on a remote box):

```
POST /admin/providers/:id/oauth/start
  → generates PKCE verifier + state, stores in provider_oauth_state, returns { authorize_url }
  (user opens authorize_url in any browser, consents; OpenAI redirects to the fixed localhost:1455
   URL, which fails to load since nothing's listening there — but the `code` param is visible
   right in the browser's address bar)

POST /admin/providers/:id/oauth/complete   { "code": "..." }
  → exchanges code + stored verifier for tokens (form-urlencoded body, per OpenAI's token
    endpoint) against auth.openai.com; the response's id_token is a JWT whose
    `https://api.openai.com/auth` claim contains chatgpt_account_id/workspace_id — this MUST be
    decoded at this step and written into provider_data, it is not returned as a plain field.
    Persists access/refresh/id token, provider becomes usable.
```

**Background refresh** (one tokio task in `providers/`, JSON-bodied refresh request per Codex's
token endpoint — note this differs from the form-urlencoded code exchange): proactively refreshes
any `oauth_codex` provider's tokens ~5 days before the refresh token's ~8-day max age (9router's
numbers). This is a *supplement* to, not a replacement for, reactive refresh-on-401: a token can
be revoked between background ticks, so the regular request path must still check
`needs_refresh`/attempt a refresh on a 401 before falling over to the next provider. Both paths
share a **refresh lock keyed by `provider_id`** so a background tick and a request-triggered
refresh can't race and both attempt to exchange the same (single-use) refresh token — the second
one to arrive must wait for and reuse the first's result rather than refreshing again. An
unrecoverable refresh error (e.g. `invalid_grant`) marks the provider `Misconfigured` and requires
the user to redo the OAuth flow.

**Pool composition:** a Codex provider's `wire_format` is `openai`, so it can sit in the same pool
as ordinary OpenAI-passthrough providers and fail over between them transparently — the adapter's
whole job is making Codex *look like* a normal OpenAI-shaped provider from the client's side.

## Admin API

All under `/admin/*`, same shared-secret auth (v1 has one secret for both proxy and admin).

```
POST/GET/PATCH/DELETE  /admin/providers            (api_key masked in responses)
POST                   /admin/providers/:id/test    connectivity/credential check
POST                   /admin/providers/:id/oauth/start     Codex OAuth: returns authorize_url
POST                   /admin/providers/:id/oauth/complete  Codex OAuth: exchanges code for tokens
POST/GET/PUT/DELETE    /admin/pools , /admin/pools/:id/members
GET                    /admin/export                full config dump (providers+pools) as JSON
POST                   /admin/import                 load config from JSON (also used for first-boot seed)
GET                    /admin/stats , /admin/stats/pools/:id
GET                    /admin/providers/:id/state    current backoff_level / status / unavailable_until
GET                    /health                       unauthenticated liveness/readiness
```

Export/import is treated as essential, not optional — with API-only config and no UI, it's what
makes the router backup-able, version-controllable, and recoverable, and enables seeding config
on first boot from a checked-in file.

## Error handling

- Bad/missing model, pool, or malformed body → `400`, error JSON shaped to match the wire
  format's own conventions (OpenAI-shaped error for `/v1/chat/completions`, Anthropic-shaped for
  `/v1/messages`) so SDK clients parse it correctly instead of choking.
- Auth failure → `401`
- All providers unavailable/misconfigured → `503` + last real upstream error + retry hint
- Malformed upstream response → relay raw status+body untouched, no reinterpretation

## Observability

- **Structured JSON logs to stdout** via `tracing`/`tracing-subscriber`, one line per request
  attempt (pool, provider, status, latency, failover chain, backoff transitions) — this is the
  actual incident-debugging surface, not `request_log` in SQLite which isn't tailable live.
  Per-request span/trace ID from day one.
- **Secret redaction** — `api_key` and the shared bearer secret must never appear in logs.
- **`/health`** unauthenticated endpoint for Docker/systemd liveness probes, and to answer "is it
  up and does any pool have a live provider" in one curl.
- **Graceful shutdown** with a drain timeout (`axum::serve` + `with_graceful_shutdown`) so
  in-flight SSE streams aren't cut off mid-response on deploy/restart.

## Deployment

Single static binary (musl target) + Dockerfile (scratch/distroless base). Bootstrap config via
env vars: listen address, sqlite path, shared secret, optional path to a seed-config JSON file
for first boot (paired with `/admin/import`).

## Testing

- **Unit tests:** pool selection, backoff rule-table matching, cooldown state transitions — pure
  functions, no I/O.
- **Integration tests:** axum app + mocked upstream (`wiremock` crate) covering: failover on
  429/5xx, streaming passthrough correctness/backpressure, and specifically **a fixture that
  streams an error event over an HTTP 200** (the known accepted-limitation case above) to confirm
  it's logged distinctly rather than silently mishandled.
- **E2E phase (new):** after the integration test suite and a manual smoke test both pass, run an
  additional e2e phase against **real provider APIs**, using a small set of sample keys the user
  provides. This is a distinct, later phase — not part of the fast unit/integration loop (costs
  money, can be rate-limited) — but is required before calling a release done. Exercises both
  `/v1/chat/completions` and `/v1/messages` passthrough against at least one real provider each,
  plus a real failover scenario (e.g. an intentionally invalid key in a pool ahead of a valid one).
  Also exercises the Codex adapter end-to-end against a real ChatGPT account: the OAuth
  start/complete flow, one real chat request through the Responses-API transformation, and
  (if feasible without waiting days) a manual trigger of the refresh path.

## Explicit non-goals for v1

- Per-client virtual API keys / usage-based billing (schema already leaves room via a future
  `keys` feature module, but not built now)
- Format translation between OpenAI/Anthropic/Gemini/etc. shapes — both supported routes are
  pure passthrough for `kind = 'passthrough'` providers; a pool must be homogeneous in
  client-facing wire format. The Codex adapter is the sole, deliberate exception.
- Combo/fusion multi-model panel requests
- OAuth/token-refresh flows for any provider other than Codex (e.g. Gemini-CLI, Antigravity,
  Qwen — 9router supports these via the same generalized pattern Codex uses, but they're
  explicitly deferred; the `ProviderAdapter` trait leaves room to add them later without
  touching the passthrough path)
- Built-in web dashboard UI (admin is API-only for v1)
- More routing strategies beyond priority-ordered failover (round-robin/least-latency deferred;
  `pool_members.priority` already leaves room)

## Review process note

This design was reviewed by two independent Opus subagents (Technical Architecture, Product
Design) plus a research pass that cloned 9router and used graphify to extract concrete schema,
module-organization, and backoff-logic precedent from its codebase. All three sets of findings
are incorporated above rather than kept as separate documents.
