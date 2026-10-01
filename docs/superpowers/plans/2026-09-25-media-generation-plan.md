# Media Generation (Image + Video) — Design & Implementation Plan

> **2026-09-25: image scope superseded** by
> `2026-09-25-image-generation-codex-plan.md` (ChatGPT/Codex first).
> Video sections here remain the reference for a later phase.

> **Status:** REVISED after an independent Opus review (2026-09-25).
> Nothing here is implemented. The review found 6 blockers (silent
> data loss on import, wrong media URL derivation, boot-time chat-pool
> auto-creation, unspecified wire_format/modality selection, OpenAI Video
> object mismatch, multipart submits) and a factually wrong failover table;
> all are fixed in the task bodies below and summarized in
> "Review revisions" at the end. Where this plan is unusually specific
> ("not X, Y instead"), that's usually a review catch — don't relax it.

**Goal:** Let `/v1/*` callers generate images and videos through 1router,
reusing the existing auth, users, providers, pools, runtime backoff and
telemetry — without changing a single byte of behavior on the existing
chat/code path (`/v1/chat/completions`, `/v1/messages`).

**Decision (settled, not re-litigated here):** same repo, same binary, new
`src/media/` module. No fork. If video ever needs independent scaling, split
into a second binary in a cargo workspace sharing a `core` crate — still not
a fork.

**Architecture in one paragraph:** A pool gains a `modality`
(`chat` | `image` | `video`, default `chat`), and `pools::select::select()`
filters on it exactly the way it already filters on `wire_format`, so a chat
route can never resolve to an image pool and vice versa. A provider gains a
nullable `media_api` saying which upstream media dialect it speaks (`NULL` =
not media-capable). New routes live in `src/media/`: images are a
synchronous, streamed-through passthrough with a *billing-aware* failover
policy; videos are asynchronous jobs persisted in a new `media_jobs` table,
advanced by a background poller, and owned by the `Caller` that created
them. Media traffic uses its own `reqwest::Client` with longer timeouts.
The whole thing is gated by a runtime setting `media_enabled` (default
off), so the binary can ship to the live gateway dark.

**Tech stack:** existing deps only (`axum 0.7`, `reqwest 0.12` with
`stream`, `sqlx`/sqlite, `tokio`, `serde_json`, `uuid`, `futures`,
`bytes`). **No new crate in P0–P4.** (Multipart image edits in P5 may need
`axum`'s `multipart` feature — that's a feature flag, not a new crate.)

---

## 1. Why the chat path can't just be extended

Facts from the current tree that drive the design:

| Area | Current behavior (file) | Why it breaks for media |
|---|---|---|
| HTTP client | One shared client, `read_timeout = idle_timeout` (default **120s**), no overall timeout; `ttfb_timeout` (60s) is **declared but not enforced** (`core/http_client.rs`, `core/config.rs`) | A non-streaming image call sends zero bytes until done. High-res / `n>1` / slow vendors can exceed 120s of silence → killed as a read timeout. Video downloads are large and long. |
| Failover | `proxy/backoff.rs::classify`: 401 → AuthExpired (refresh only for OauthCodex, `flow.rs:~326`); 400/413/422 → NonRetryable; 408/429/5xx → Retryable; **everything else (403, 404, 3xx) → Retryable** with 30s cooldown. Transport errors after send are also retried. | For media, a 500/504/timeout *after send* may already be billed; retrying double-bills. Needs its own classifier. |
| `base_url` | It is the **full endpoint URL**, POSTed as-is (`adapter/http.rs:93-99`), e.g. `https://api.openai.com/v1/chat/completions` | `{base_url}/images/generations` would be wrong. Must derive like `derive_models_url` (`providers/routes.rs:387`). |
| Boot | `ensure_direct_pools_for_unassigned_providers` (`core/state.rs:141`, every boot) auto-creates a pool per member-less provider | Would create a *chat* pool for a media provider. |
| Import/seed | `admin/mod.rs::import_config` uses explicit-column SQL (`INSERT INTO pools (id, wire_format, created_at)`, line ~109); `seed.rs` reuses it | `#[serde(default)]` alone is not enough — new columns would be silently dropped on import. (It already drops `strategy`/`sticky_limit` today — existing bug.) |
| Response handling | `transform_response` for SSE/JSON, aggregation capped by `MAX_BUFFERED_BODY` = 64 MiB | `b64_json` images with `n=4` at high res can be tens of MB; video is binary. Must stream through, never buffer. |
| Dataset tee | `proxy/dataset_tee.rs` tees full request/response bytes to JSONL | Base64 images / video bytes in JSONL would explode disk. |
| Open access | `require_bearer` lets anonymous callers through when `require_shared_secret` is off (`auth/middleware.rs`) | Anonymous chat is cheap-ish; anonymous video generation is a money leak. |
| Pool pinning | Pool is pinned by `wire_format` only | Nothing stops `model: "<image-pool>"` from being sent to `/v1/chat/completions`. |
| `flow.rs` | ~800 lines, tightly bound to `client_wanted_stream` / SSE / embedded-error-on-200 logic | Adding media branches here raises regression risk on the live path. |

Correction to the earlier chat answer: I said "ttfb 5s" — that's the
test-fixture value; production defaults are ttfb 60s (unenforced) and idle
120s. The conclusion (media needs its own client) stands.

---

## 2. Client-facing API (v1)

OpenAI-compatible shapes, so existing SDKs work unchanged.

### Images (synchronous)

- `POST /v1/images/generations` — body is OpenAI's
  (`model`, `prompt`, `n`, `size`, `quality`, `response_format`, …).
  `model` = image pool id, or `<provider_id>/<model>` direct addressing
  (same rules as chat, including `direct_model_allowed`).
- Response: upstream JSON streamed through unchanged (no re-serialization),
  plus the existing debug headers; response headers via the existing
  `relay_upstream_header` allowlist (SEC-07).
- `stream: true` (gpt-image SSE / `partial_images`) → **400 in v1**.
- Don't inject or rewrite `response_format`; forward what the client sent
  (gpt-image models reject it and always return `b64_json` — unverified,
  from secondary docs; confirm against the real upstream in P2).
- **Out of v1:** `/v1/images/edits` and `/v1/images/variations` (multipart
  upload) → P5.

### Videos (asynchronous job), modeled on OpenAI's Videos API

- `POST /v1/videos` — **JSON only in v1**; `multipart/form-data`
  (`input_reference` upload) → **415**. Returns a Video object.
- `GET /v1/videos/:id` → current job state (from `media_jobs`, never a live
  upstream call on the request path)
- `GET /v1/videos/:id/content` → streams the finished video
  (`?variant=video|thumbnail|spritesheet` passed through)
- `GET /v1/videos` → caller's own jobs, paginated (`limit`, `after`)
- `DELETE /v1/videos/:id` → OpenAI semantics are *delete the asset*, not
  cancel generation. Locally: stop polling, best-effort upstream DELETE,
  mark the row `failed` with `error.code = "deleted"` (row kept for stats).

The `id` returned to the client is **1router's own id** (`vid_<uuid>`),
never the upstream job id — the upstream id is internal and provider-scoped.

**Video object must be a strict superset of OpenAI's** or SDKs fail to
deserialize: `id`, `object: "video"`, `model`, `status` ∈
{`queued`,`in_progress`,`completed`,`failed`} **only** (no `cancelled`),
`progress`, `seconds` (**string**, e.g. `"8"`), `size`, `created_at` /
`completed_at` / `expires_at` (**unix seconds**), `error` (`{code, message}`
or null), `remixed_from_video_id` (null in v1). `expires_at` is taken from
the upstream, not a local TTL.

**Idempotency:** honor an optional `Idempotency-Key` header on
`POST /v1/videos` (unique per `user_id`, 24h) → a retried submit returns
the existing job instead of billing twice. Stored in
`media_jobs.idempotency_key`.

### `/v1/models`

`proxy/routes.rs:71-96` lists every pool and every discovered
`<provider>/<model>`. Change: list only `modality = chat` pools and skip
discovered models of providers with `media_api IS NOT NULL`; list media
pools **only when `media_enabled` is on**. No non-standard field.

### Errors

Always OpenAI-shaped (`proxy::error_response::wire_error(WireFormat::OpenAi, …)`).
When `media_enabled` is off, media routes return **404** (indistinguishable
from the feature not existing).

---

## 3. Data model — migration `0009_media.sql`

```sql
-- Pools answer exactly one route family. Default keeps every existing pool chat.
ALTER TABLE pools ADD COLUMN modality TEXT NOT NULL DEFAULT 'chat';

-- Which upstream media dialect a provider speaks. NULL = not media-capable.
-- v1 values: 'openai_images', 'openai_videos'. (P5: 'google_veo', 'fal', …)
ALTER TABLE providers ADD COLUMN media_api TEXT;

CREATE TABLE media_jobs (
    id               TEXT PRIMARY KEY,          -- vid_<uuid>, client-facing
    user_id          TEXT,                      -- Caller.user_id; ownership key
    pool_id          TEXT,                      -- NULL for direct addressing
    provider_id      TEXT NOT NULL,             -- bare string, like request_log
    model            TEXT NOT NULL,             -- effective upstream model
    upstream_job_id  TEXT NOT NULL,
    status           TEXT NOT NULL,             -- queued|in_progress|completed|failed (OpenAI set only)
    idempotency_key  TEXT,                      -- optional client Idempotency-Key
    upstream_expires_at TEXT,                   -- from upstream Video object
    progress         INTEGER,                   -- 0-100 if upstream reports it
    request_json     TEXT NOT NULL,             -- client body, capped at 16 KiB
    error            TEXT,                      -- capped at 4 KiB
    units            REAL,                      -- billed seconds, when known
    next_poll_at     TEXT NOT NULL,
    poll_attempts    INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    completed_at     TEXT,
    expires_at       TEXT                       -- after this, /content returns 410
);
CREATE INDEX idx_media_jobs_user   ON media_jobs(user_id, created_at);
CREATE INDEX idx_media_jobs_poll   ON media_jobs(status, next_poll_at);
CREATE UNIQUE INDEX idx_media_jobs_idem ON media_jobs(user_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Attribute media traffic in the existing stats.
ALTER TABLE request_log ADD COLUMN modality TEXT;   -- NULL = chat (pre-feature rows)
ALTER TABLE request_log ADD COLUMN units REAL;      -- images generated / video seconds
ALTER TABLE request_log ADD COLUMN usage_json TEXT; -- upstream `usage` (gpt-image token counts), capped
```

Media pools still need a value for the NOT NULL `pools.wire_format`: store
`openai` and **ignore it** for non-chat modalities (see §5.1).

Rust side:

- `core::model::Modality { #[default] Chat, Image, Video }` (`sqlx::Type`,
  snake_case, `#[derive(Default)]` — required for `serde(default)` to
  compile), `Pool.modality` with `#[serde(default)]`.
- `core::model::MediaApi { OpenAiImages, OpenAiVideos }`,
  `Provider.media_api: Option<MediaApi>` with `#[serde(default)]`.
- `#[serde(default)]` is mandatory on both — `admin/mod.rs` export/import and
  `seed.rs` deserialize these structs directly from pre-existing files (the
  same trap the dataset-logging plan hit; see its revision note).
- **`#[serde(default)]` is necessary but not sufficient.** Every
  explicit-column SQL path must bind the new columns, or they are silently
  dropped:
  - `admin/mod.rs::import_config` pools INSERT **and** UPDATE (also fix the
    existing drop of `strategy`/`sticky_limit` while there) and providers
    INSERT/UPDATE; `seed.rs` goes through the same function.
  - `providers/queries.rs::insert_provider` (~:122), `update_provider`
    (~:213), `ProviderPatch`.
  - `pools/queries.rs::insert_pool` (~:22), `CreatePool` in `pools/routes.rs`.
  - `telemetry/request_log.rs` INSERT (~:53).
- `core/state.rs::ensure_direct_pools_for_unassigned_providers`: add
  `AND p.media_api IS NULL` to **both** statements, so boot never
  auto-creates a chat pool for a media provider.
- `LogEntry` gains `modality`, `units`, `usage_json` (all `Option`); every
  existing construction site (~7, grep `LogEntry {`) passes `None`.
- Before touching any struct, grep all literal construction sites
  (`Provider {`, `Pool {`, `LogEntry {`, `AppState {`, `Config {`) in `src`
  and `tests` — there are dozens; do not trust a hardcoded list.

---

## 4. Module layout

```
src/media/
  mod.rs         routes() + enabled-gate middleware
  routes.rs      handlers for /v1/images/* and /v1/videos*
  client.rs      build_media_client(&Config)
  select.rs      thin wrapper: pools::select with Modality + media_api filter
  failover.rs    billing-aware retry classification
  images.rs      sync passthrough flow
  jobs.rs        media_jobs queries (create / get-owned / list-owned / update)
  poller.rs      background task advancing in-flight video jobs
  adapter/
    mod.rs       MediaAdapter trait + adapter_for(MediaApi)
    openai_images.rs
    openai_videos.rs
```

`src/app.rs`: merge `media::routes()` under the same `require_bearer`
route_layer as `proxy`, plus the `media_enabled` gate and the
"authenticated caller required" check (see §6).

---

## 5. Key mechanics

### 5.1 Selection

**Do not change `select()`'s signature** (one production caller,
`proxy/flow.rs:141`, but ~20 unit tests call it). Instead:

- Add `pools::select::select_for_modality(snapshot, pool_id, modality,
  rotation)`. It checks `pool.modality == modality` **before**
  `rotate_from_cursor` (so a rejected request never advances round-robin
  state) and, for non-chat modalities, **ignores `wire_format`**.
- `select()` becomes a thin wrapper: its existing wire_format check, plus
  reject when `pool.modality != Chat` — also before rotation. Chat
  behavior for every existing pool (all default `chat`) is unchanged.

Direct addressing (`select_direct_provider`) has no pool, so for media
routes additionally require `provider.media_api` to match the route
(`OpenAiImages` for images, `OpenAiVideos` for videos); chat routes ignore
`media_api`. Media pool members whose provider has a non-matching or NULL
`media_api` are skipped at selection time, and rejected at write time in
`pools/queries.rs::put_member` (~:126). `providers/queries.rs::update_provider`
gets a `media_api` stranding guard mirroring the existing `wire_format`
guard (can't change `media_api` while the provider is a member of a pool
whose modality would no longer fit).

**Media URL derivation.** `base_url` is a full endpoint. Add
`media::url::derive_media_url(base_url, "images/generations" | "videos")`
using the same suffix-strip-then-swap-last-segment logic as
`derive_models_url` (`providers/routes.rs:387`) — factor the shared bit out
rather than duplicating it. Admins keep entering the same URL they use for
chat; do **not** ask for an API-root base_url (breaks discovery).

**Chat-only provider tools.** `validate_model` (sends a chat "hi"),
`test_stub` (GET base_url) and model discovery assume chat. For providers
with `media_api` set: skip the chat probe in `validate_model` and return
"not validated for media" (v1), keep discovery (OpenAI `/models` lists
image/video models too, which `direct_model_allowed` relies on).

Media routes are **Passthrough-kind providers only** in v1. Codex / Command
Code OAuth providers are rejected for `media_api` at the admin API.

### 5.2 Reusing chat helpers (no refactor — P0 cut)

Nothing needs extracting: `credentials_for` is already `pub(crate)`
(`proxy/flow.rs:25`, already used by `providers/routes.rs`); the refresh
lock lives in `providers/refresh_lock.rs`; runtime bookkeeping is inline
`state.runtime.entry(runtime_key(..)).record_*()` calls that media repeats
directly. Media calls these as they are, so **the live chat path's
`flow.rs` is not touched at all.** OAuth refresh is irrelevant anyway:
media is Passthrough-only in v1.

Runtime keys: media uses the existing `runtime_key(provider_id, model)`,
so a provider that's cooling for its image model doesn't affect its chat
model, and vice versa. (Same map, distinct keys.)

### 5.3 Media HTTP client

`media::client::build_media_client(cfg)`: same builder as
`core::http_client::build_client` (**including `redirect(Policy::none())`,
SEC-10**) but:

- `read_timeout = ROUTER_MEDIA_IDLE_TIMEOUT` (default **300s**)
- plus an overall per-attempt `tokio::time::timeout` of
  `ROUTER_MEDIA_REQUEST_TIMEOUT` (default **600s**) around images calls
  (the send + full body stream)

Stored as `AppState.media_http`. New `Config` fields for both env vars.

### 5.4 Billing-aware failover (`media/failover.rs`)

Classify each attempt's outcome into:

A **separate** classifier — not `proxy/backoff.rs::classify`, whose
catch-all treats 403/404/3xx as retryable and whose 5xx retry is fine for
chat but not for billed media.

| Outcome | Retry next member? | Runtime state |
|---|---|---|
| Connect refused / DNS / TLS (request never sent) | yes | `record_retryable` |
| 429, 503 | yes | `record_retryable` (honor `retry-after`) |
| 401, 403 (no refresh — Passthrough only) | yes | `mark_misconfigured` |
| 404 (model/route unknown on this provider — nothing billed) | yes | `record_retryable` |
| 3xx (never followed, SEC-10) | yes | `record_retryable` |
| 400, 413, 422 (bad prompt, content policy, bad size) | **no** — relay | none |
| 408, 500, 502, 504, other 5xx | **no** — relay (may be billed) | `record_retryable` |
| Read timeout / stream error *after send* | **no** — 504 `upstream_timeout_possibly_billed` | `record_retryable` |
| 2xx | done | `record_success` |

Rule: fail over only when we're confident the upstream did no billable
work. (Q3 settled: no retry on 5xx.)

Streaming a 2xx body through: once the first byte of a 2xx body has been
forwarded to the client, no retry is ever possible (same as chat).

### 5.5 Images flow (`media/images.rs`)

1. Buffer the request body (`proxy::body::buffer_body`, existing
   `max_body_bytes` cap — JSON generations are small).
2. Parse `model`, and `n` for accounting (cap `n` at `ROUTER_MEDIA_MAX_N`,
   default 4 → 400 above it).
3. `select(…, Modality::Image)`, direct-model check for non-admin callers.
4. For each member: skip if runtime-unavailable; `MediaAdapter::build_request`
   (swap `model` to effective model, set auth header, target
   `{base_url}/images/generations`); send via `media_http`; classify.
5. On 2xx: return the upstream body **as a stream**
   (`Body::from_stream(resp.bytes_stream())`) with upstream `content-type`,
   plus a hard output cap (`ROUTER_MEDIA_MAX_RESPONSE_BYTES`, default
   256 MiB) enforced by a counting stream wrapper that errors past the cap.
6. `request_log`: `modality = image`, `units = n`, latency, user_id, and
   `usage_json` = upstream `usage` object if present. Since the body is
   streamed, parse `usage` from a bounded tail/scan of the stream in the
   counting wrapper (JSON is small except `b64_json`; skip strings) — or,
   if that's too fiddly, cut `usage_json` to P5 and log `n` only.
7. Dataset logging: **off for media in v1**, regardless of provider/member
   toggles (documented; revisit with a metadata-only record later).

### 5.6 Video jobs flow

**Submit (`POST /v1/videos`):**
1. Buffer body; parse `model`; `select(…, Modality::Video)`.
2. Failover across members using the §5.4 table — submit is a short call,
   so failover here is normal.
3. On upstream 2xx: extract upstream job id, insert `media_jobs` row
   (`status` from upstream or `queued`, `next_poll_at = now + 5s`), return
   the 1router-shaped job object with our `vid_…` id.
4. Once a job row exists, the job is **pinned to that provider forever** —
   never resubmitted elsewhere (would double-bill). A submit that got an
   upstream 2xx whose body we fail to parse, or a timeout after send, is a
   hard error to the client (no failover).
5. Per-user cap on concurrently active jobs (`ROUTER_MEDIA_MAX_ACTIVE_JOBS`,
   default 5) → 429 above it.

**Poller (`media/poller.rs`, spawned in `main.rs` next to `refresh_task`):**
- Every 2s, `SELECT … WHERE status IN ('queued','in_progress') AND
  next_poll_at <= now LIMIT 20`.
- For each: `MediaAdapter::poll(upstream_job_id)`; update status/progress;
  backoff `next_poll_at` 5s → 10s → 20s → 30s cap; on terminal status set
  `completed_at`, `units`, `expires_at`.
- Hard ceiling `ROUTER_MEDIA_JOB_MAX_AGE` (default 2h) → `failed` with
  `error = "timed out waiting for upstream"`.
- Poll errors: transient → just back off; 404 from upstream → `failed`.
- Concurrency: bounded (`buffer_unordered(4)`). Single-process assumption
  (SQLite, one binary) — documented; no distributed lock.
- **Every state write is compare-and-swap:** `UPDATE media_jobs SET …
  WHERE id = ? AND status = ? AND updated_at = ?`; 0 rows affected → someone
  else (another tick, DELETE) got there first, drop the result. Single-row
  statements only, no long transactions — DB is WAL, 5-connection pool,
  5s busy_timeout, and the `request_log` writer batches 100 rows/tx.
- Poll interval and max age come from `Config`, and the poller exposes a
  `tick_once(&AppState)` so tests can drive it deterministically.
- Restart-safe: all state is in the DB; the poller resumes on boot.
- `request_log` row written once per job at terminal state
  (`modality = video`, `units = seconds`).

**Read (`GET /v1/videos/:id`, `GET /v1/videos`):** DB only, never a live
upstream call. **Ownership check:** two query variants chosen in Rust —
`caller.is_admin()` → `WHERE id = ?`; otherwise `WHERE id = ? AND user_id = ?`.
Not a SQL `OR ? = 'admin'` trick. Not-owned → **404** (not 403 — don't
confirm existence).

**Content (`GET /v1/videos/:id/content`):** ownership check → status must be
`completed` → not past `upstream_expires_at` (else 410) →
`MediaAdapter::build_content(...)` (OpenAI: `GET /videos/{upstream_id}/content`
with the provider's key) → stream through with upstream `content-type` +
`content-length` (via the SEC-07 relay allowlist, extended with
`content-length`), under the same output cap. **v1 stores nothing locally.**

If a future adapter returns a *signed URL* to download from instead of an
authenticated endpoint: fetch it with a separate client that sends **no
provider credentials**, allows redirects only to https, and rejects
private/loopback/link-local IPs (SSRF). Not needed for `openai_videos`;
spelled out so P5 adapters don't skip it. (A video `image_url` reference
the *client* passes is fetched by the upstream, not by us — not an SSRF
surface for the gateway.)

**Delete:** see §2 — stop polling, best-effort upstream DELETE, status
`failed` + `error.code = "deleted"` via CAS; row kept for stats.

### 5.7 MediaAdapter trait

```rust
#[async_trait::async_trait]
pub trait MediaAdapter: Send + Sync {
    /// Images: the full upstream request. Videos: the submit request.
    async fn build_request(&self, body: &Bytes, model: &str, creds: &Credentials)
        -> Result<reqwest::Request, AppError>;
    /// Videos only: extract upstream job id + initial status from submit 2xx.
    fn parse_submit(&self, body: &[u8]) -> Result<UpstreamJob, AppError>;
    async fn build_poll(&self, upstream_id: &str, creds: &Credentials)
        -> Result<reqwest::Request, AppError>;
    fn parse_poll(&self, body: &[u8]) -> Result<UpstreamJob, AppError>;
    async fn build_content(&self, upstream_id: &str, variant: Option<&str>, creds: &Credentials)
        -> Result<reqwest::Request, AppError>;
    async fn build_cancel(&self, upstream_id: &str, creds: &Credentials)
        -> Option<Result<reqwest::Request, AppError>>;
}
```

Separate from `ProviderAdapter` on purpose: none of `transform_response`'s
SSE/wire-translation semantics apply, and forcing media into that trait
would leak into every chat adapter.

---

## 6. Security

- **No anonymous media.** Media routes require `Caller.user_id.is_some()`
  even when open-access mode is on → 401 otherwise. (Open question Q2.)
- **No anonymous media** is an explicit check in the media handlers:
  `require_bearer` passes anonymous callers through in open-access mode
  (`auth/middleware.rs:117-149`).
- **IDOR:** every `/v1/videos/:id*` handler filters by owner in the SQL
  itself (admin vs. owner query variant chosen in Rust, §5.6), never
  fetch-then-check.
- **Revoked users:** a revoked user's key already fails `require_bearer`,
  so they lose access to their jobs; the poller keeps polling them to a
  terminal state for accounting; admin can still see them.
- **Output caps** on every streamed body (images and video content).
- **`n` cap** and body-size cap on submits.
- **No redirects** on the media client (SEC-10 preserved); signed-URL
  download rules above for future adapters.
- **Error relay** capped at `MAX_ERROR_BODY` (SEC-16), same helper.
- **request_json** stored capped (16 KiB) — prompts are user content;
  covered by the same trust model as dataset logs. Cleanup of old
  `request_json` deferred to P5.
- **Per-user rate limit / quota:** out of v1 (open question Q1), but the
  `request_log.units` column exists so it can be added without migration.

---

## 7. Admin API + UI

- Pools: `modality` on create (immutable after, like `wire_format`);
  pool-member add rejects providers whose `media_api` doesn't fit.
- Providers: `media_api` select (None / OpenAI Images / OpenAI Videos),
  Passthrough only.
- Settings: `media_enabled` toggle (stored via `core::settings::set_bool`,
  mirrored into an `AtomicBool` on `AppState` like `require_shared_secret`).
- Media Jobs admin page: deferred to P5 (admin can use `GET /v1/videos`
  with the shared secret meanwhile).
- Stats: split request counts/units by `modality`.
- Export/import: new fields round-trip (`#[serde(default)]` + test).

---

## 8. Phases & tasks

Each phase ends green: `cargo test --offline` + frontend tests. **Deploy to
the live gateway once, after P4** (per standing preference: finish and test
everything, deploy once).

### P0 — cut (see §5.2)

### P0.5 — Test harness
- [ ] `tests/common/mod.rs`: `spawn_app` takes a `Config` override (media
      timeouts, poll interval) and returns the `AppState`; background tasks
      are not started — tests call `media::poller::tick_once`.
- [ ] Budget: `AppState {` / `Config {` are literally constructed in ~18
      places (main.rs, app.rs tests, tests/common, several test files) —
      grep and update all when adding `media_http` / `media_enabled` /
      media config fields.

### P1 — Data model & selection
- [ ] `migrations/0009_media.sql` (§3).
- [ ] `Modality` (with `Default`), `MediaApi`, new fields with
      `#[serde(default)]`; update all construction sites (grep first).
- [ ] Bind new columns in **every** explicit-column SQL path (§3 list),
      including `import_config` INSERT+UPDATE; fix the existing
      `strategy`/`sticky_limit` drop there too.
- [ ] `ensure_direct_pools_for_unassigned_providers` excludes media providers.
- [ ] `select_for_modality` + `select()` chat-only guard, both before rotation.
- [ ] `put_member` compatibility check; `update_provider` `media_api`
      stranding guard; Passthrough-only for `media_api`.
- [ ] `/v1/models` filtering (§2).
- [ ] `derive_media_url` (shared with `derive_models_url`).
- [ ] `media_enabled` setting + `AppState` flag; `media_http` client;
      new `Config` fields.
- [ ] Tests: chat route rejects image pool; image route rejects chat pool;
      rejected selection does **not** advance round-robin cursor; old export
      file imports with defaults; export → import round-trip keeps
      `modality`, `media_api`, `strategy`, `sticky_limit`; boot with a
      member-less media provider creates no pool; `/v1/models` hides media
      pools when disabled; `put_member` rejects mismatch; URL derivation
      for `/v1/chat/completions`, `/v1/messages`, bare host.

### P2 — Images
- [ ] `media/adapter/openai_images.rs`, `media/images.rs`, route, gate,
      auth-required check.
- [ ] Failover classifier + unit tests for every row of §5.4.
- [ ] wiremock integration tests (`tests/media_images.rs`): happy path
      streamed; 429 → failover; 404 → failover; 400 → no failover;
      500 → no failover; slow upstream (> chat idle timeout, < media
      timeout, via Config override) succeeds; post-send timeout → 504,
      no second upstream call; output cap trips; `stream:true` → 400;
      anonymous in open-access mode → 401; `media_enabled=false` → 404;
      request_log row has modality/units; upstream URL is the derived
      `/images/generations`.

### P3 — Video jobs
- [ ] `media_jobs` queries with owner-scoped SQL.
- [ ] `openai_videos` adapter; submit / get / list / content / delete routes.
- [ ] Poller task (CAS writes, `tick_once`) + backoff + max-age + restart
      resume; per-user active-job cap; `Idempotency-Key`.
- [ ] Tests (`tests/media_videos.rs`): submit → tick → completed →
      content streams; response deserializes as OpenAI's Video object
      (string `seconds`, unix timestamps, status set); multipart → 415;
      user B gets 404 on user A's job (all endpoints); admin can read all;
      job pinned to provider (poll failure never resubmits); same
      `Idempotency-Key` twice → one upstream submit; active-job cap → 429;
      concurrent tick + DELETE → exactly one wins; max-age → failed; past
      `upstream_expires_at` → 410; poller resumes a job after "restart"
      (new AppState, same sqlite file); exactly one request_log row per job.

### P4 — UI, stats, docs
- [ ] Providers/Pools/Settings UI fields; stats split by modality.
- [ ] Frontend tests for new fields (and reorder doesn't wipe new fields —
      the dataset-logging reorder bug class).
- [ ] `docs/ARCHITECTURE.md` + README section; env var list.
- [ ] Build release, run e2e smoke against one real provider, then the
      single live deploy with `media_enabled` **off**, flip on after smoke.

### P5 — Later (not in this plan's scope)
- Image edits/variations and video `input_reference` (multipart),
  image SSE streaming, local storage for video with TTL, more vendors
  (Google Veo, fal, Replicate), per-user quotas, metadata-only dataset
  records, webhooks instead of polling, Media Jobs admin page,
  `request_json` cleanup, media-specific `validate_model` probe.

---

## 9. Open questions for the owner

Plan currently assumes the recommendation for each; owner to confirm.

- **Q1:** Quota? → attribution + stats only, plus a per-user cap on
  concurrent active video jobs. Real quota in P5 using `units`.
- **Q2:** Block anonymous media in open-access mode? → yes.
- **Q3:** Retry on 5xx? → no (table in §5.4).
- **Q4:** Modality in `/v1/models`? → no custom field; filter instead (§2).
- **Q5 (blocking for P2/P3 adapters):** which real upstream first —
  OpenAI directly, or a relay/other vendor whose statuses, multipart and
  expiry may differ? Confirm before building adapters.

---

## Review revisions (Opus review, 2026-09-25)

Blockers fixed:
1. Import/seed explicit-column SQL would drop new columns → bind in every
   path, round-trip test (§3, P1).
2. `base_url` is a full endpoint → `derive_media_url` (§5.1).
3. Boot auto-created chat pools for media providers → excluded (§3).
4. wire_format vs modality unspecified → `select_for_modality`, check
   before rotation, `select()` signature unchanged (§5.1).
5. Video object shape didn't match OpenAI (statuses, string `seconds`,
   unix timestamps, upstream `expires_at`) → strict superset (§2).
6. Multipart video submit → 415 in v1 (§2).

Should-fix applied: P0 refactor cut (helpers already reusable); failover
table rewritten against real `backoff.rs` semantics with a separate media
classifier; all explicit-column SQL listed; `put_member` / `update_provider`
guards; `/v1/models` filtering; chat-only provider tools handled; CAS poller
writes + `tick_once`; DELETE semantics; `Idempotency-Key`; test harness
phase; `usage` capture; SEC-07 header relay.

Cuts: Media Jobs page, `request_json` cleanup → P5. Nits: ownership query
chosen in Rust, `Modality: Default`, `stream:true` → 400.

Unverified (confirm in P2 against the real upstream): gpt-image always
returns `b64_json` and rejects `response_format`; gpt-image SSE streaming
behavior; dall-e-3 `n=1` limit.
