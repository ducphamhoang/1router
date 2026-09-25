# Image Generation via ChatGPT (Codex OAuth) — Implementation Plan

> **Scope decision (owner, 2026-09-25):** images first, ChatGPT/Codex OAuth
> first. Video is deferred. This plan **supersedes the image parts** of
> `2026-09-25-media-generation-plan.md`; that plan's reviewed lessons
> (import SQL, selection before rotation, `/v1/models` filtering,
> billing-aware failover, no anonymous media) are carried over here. Its
> video sections stay as future reference.
>
> **Reference implementation:** 9router (`decolua/9router`, MIT, HEAD
> `39e36d3`), `open-sse/handlers/imageProviders/codex.js`. We reimplement in
> Rust; no code is copied. Where we deliberately differ from 9router, it's
> called out.

**Goal:** `POST /v1/images/generations` (OpenAI Images API shape) served by
the ChatGPT accounts 1router already holds as `OauthCodex` providers — same
OAuth credentials, same refresh, same pools/failover across accounts —
without touching the chat path's behavior.

**Why this is small:** a Codex provider already has everything: OAuth
tokens, `ChatGPT-Account-ID`, background refresh (`refresh_task.rs`),
refresh-on-401 (`refresh_lock::refresh_and_persist_detached`), and the SSE
parser helpers in `codex/transform.rs`. Image generation is the same
upstream (`https://chatgpt.com/backend-api/codex/responses`) with a
different request body (`tools: [{type: "image_generation"}]`) and a
different thing extracted from the SSE stream.

**No new crate.**

> **Revised after Opus review (2026-09-25):** 4 blockers + 11 should-fix
> applied in the task bodies; summary in "Review revisions" at the end.
> Where the plan says something with unusual specificity, it's usually a
> review catch — don't relax it.

---

## 1. Upstream wire contract (from 9router, to verify in P0)

> **P0 done** — verified against a real account; read "P0 findings" at the
> end before implementing (tool params are hints only, text accompanies
> images, refusals are soft, usage lives in `tool_usage.image_gen`).

**URL:** `https://chatgpt.com/backend-api/codex/responses` (same as chat).
Today it's a private `const RESPONSES_URL` (`codex/adapter.rs:16`) — **not
overridable**, and no existing test hits `/responses`
(`tests/codex_oauth.rs` only overrides the token URL, via the
process-global, racy env var `CODEX_TOKEN_URL`). → Add
`Config.codex_responses_url` (default = the current const) and use it in
the image module, so tests can point it at wiremock. The chat adapter
keeps its const in v1 (don't touch chat).

**Headers:** `Bearer <access_token>`, `originator: codex_cli_rs`,
`ChatGPT-Account-ID`, `accept: text/event-stream`, per-request `session_id`
(uuid v4). Note: 1router's chat UA is `codex_cli_rs/{CARGO_PKG_VERSION}`
— that's **1router's** version, not a Codex CLI version. 9router sends
`codex_cli_rs/0.154.0` + `version: 0.154.0` + `x-client-request-id`. The
image tool may be gated on client version (unverified) → P0 tests both;
if needed, add `Config.codex_client_version` used by the image path.

**Body:**
```json
{
  "model": "<host chat model>",
  "instructions": "",
  "input": [{ "type": "message", "role": "user",
              "content": [{ "type": "input_text", "text": "<prompt>" }] }],
  "tools": [{ "type": "image_generation",
              "model": "gpt-image-2",          // only for gpt-image-* models
              "action": "generate",            // only for gpt-image-* models
              "output_format": "png",
              "size": "...", "quality": "...", "background": "..." }],  // only if client sent
  "tool_choice": { "type": "image_generation" },   // "auto" for -image aliases
  "parallel_tool_calls": false,
  "prompt_cache_key": "<fresh uuid v4>",   // never the chat key 1router-{id}-{model}-{wire}
  "stream": true,
  "store": false,
  "reasoning": { "effort": "medium", "summary": "auto" }   // OMITTED (not null) for -image aliases
}
```

**Model mapping** (client `model` → upstream):

| Effective model | Responses `model` | Tool `model` / `action` |
|---|---|---|
| `gpt-image-*` (e.g. `gpt-image-2`) | `ROUTER_CODEX_IMAGE_HOST_MODEL` (default `gpt-5.5`, what 9router uses) | the gpt-image name / `generate` |
| `<chat>-image` (e.g. `gpt-5.5-image`) | `<chat>` (suffix stripped) | omitted; `tool_choice: "auto"` |
| anything else | → 400 `unsupported image model` | — |

**SSE events consumed:**
- `response.output_item.done` with `item.type == "image_generation_call"` →
  `item.result` is the final base64 image; `item.revised_prompt` if
  present (9router ignores it; we pass it through — OpenAI's shape has it).
- `response.completed` → **terminal marker** (required to classify "no
  image" as entitlement vs truncation, §4); its `usage` → optional
  `usage` in our response.
- `response.image_generation_call.partial_image` → ignored in v1.
- `response.failed` / `error` → new image-specific extractor (§4). **Not**
  `transform::sse_embedded_error` — it only reads top-level
  `data.error.type` (`transform.rs:321-329`), while the Responses API
  puts it at `data.response.error.{code,message}` (`response.failed`) and
  `data.{code,message}` (`event: error`).

**Parsing:** read the whole body with `read_body_limited` (64 MiB cap),
then split events with `transform::sse_events` (make it `pub(crate)`;
currently private at `transform.rs:259`). **Do not** use the streaming
`SseFramer` — its `MAX_SSE_BUFFER` is 16 MiB (`sse.rs:11`) and a large
PNG in `output_item.done` (and again in `response.completed.output[]`)
can exceed it, making the stream end silently.

**Response to client:** `{ "created": <unix>, "data": [{ "b64_json": "…",
"revised_prompt": "…" }] }`. Codex always returns base64 and one image.

**Account entitlement:** Free ChatGPT accounts don't get images
(Plus/Pro required). Stream ends with no `image_generation_call` →
treat as "this account can't do it", fail over (§4).

---

## 2. Client-facing API (v1)

`POST /v1/images/generations`, JSON:

| Field | v1 handling |
|---|---|
| `model` (required) | image pool id, or `<codex_provider_id>/<model>` |
| `prompt` (required) | non-empty string, capped at 32 KiB → else 400 |
| `n` | absent or `1`; anything else → 400 (Codex returns one image; we don't silently loop and burn quota) |
| `size` | `auto` or `^\d{3,4}x\d{3,4}$` |
| `quality` | `low` / `medium` / `high` / `auto` |
| `background` | `transparent` / `opaque` / `auto` |
| `output_format` | `png` / `jpeg` / `webp` (default `png`) |
| (above four) | validated → 400 otherwise; only these reach the tool object (no free-form passthrough — differs from 9router) |
| `response_format` | absent or `b64_json`; `url` → 400 (we don't host files) |
| `stream: true` | 400 in v1 (P3 adds OpenAI-shaped streaming) |
| `user`, `style`, others | ignored |

Deliberate differences from 9router: no custom `Accept`-header streaming
protocol, no `?response_format=binary`, no JSON `image`/`images[]` edits on
the generations route (edits → P3 on `/v1/images/edits`), no remote-URL
fetching (SSRF surface).

Errors are OpenAI-shaped (`wire_error(WireFormat::OpenAi, …)`).

---

## 3. Data model & selection

**Migration `0009_image_pools.sql`:**
```sql
ALTER TABLE pools ADD COLUMN modality TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE request_log ADD COLUMN modality TEXT;  -- NULL = chat (old rows)
ALTER TABLE request_log ADD COLUMN units REAL;     -- images generated
```

No `media_api` column in v1: image capability is derived —
`provider.kind == OauthCodex`. (Passthrough OpenAI Images providers →
P2, which is when `media_api` gets added.) This also removes the
boot-time auto-pool problem: Codex providers stay normal chat providers,
and one ChatGPT login serves both chat pools and image pools.

**Rust:**
- `core::model::Modality { #[default] Chat, Image }` (`Default` derive
  needed for `serde(default)`), `Pool.modality` with `#[serde(default)]`.
- `LogEntry { modality: Option<Modality>, units: Option<f64>, .. }`; all
  existing sites pass `None` (grep `LogEntry {`).
- Bind `modality` in **every** explicit-column SQL path:
  `admin/mod.rs::import_config` pools INSERT + UPDATE (and fix the existing
  drop of `strategy`/`sticky_limit` in the same statements), `seed.rs` via
  the same function, `pools/queries.rs::insert_pool`, `CreatePool` in
  `pools/routes.rs`, `telemetry/request_log.rs` INSERT.

**Selection** (`select()` signature unchanged):
- New `pools::select::select_for_modality(snapshot, pool_id, modality,
  rotation)`: checks `pool.modality` **before** `rotate_from_cursor`, and
  for `Image` ignores `wire_format` (image pools store `openai`).
- `select()` additionally rejects `modality != Chat` pools, also before
  rotation → a chat request can never hit an image pool.
- Image pool members: only `OauthCodex` providers, `model_override`
  **required** and matching `gpt-image-*` or `*-image` (a Codex provider's
  own `upstream_model` is a chat model). Enforced in
  **`pools/queries.rs::upsert_member`** (`:98`; the handler is
  `pools/routes.rs::put_member` `:126`) next to the existing
  reasoning-effort validation. **Also** skipped at selection time, because
  `import_config` bypasses `upsert_member` (`admin/mod.rs:120-135`).
- Direct addressing `<codex_id>/<model>` for images: **admin (shared
  secret) only**, `provider.kind == OauthCodex` (`select_direct_provider`
  returns any kind, `select.rs:173-195`), model matches `gpt-image-*` or
  `*-image`. Non-admins use image pools. (Q2.)
- **Chat-side leak fix:** `direct_model_allowed` (`select.rs:204-219`)
  accepts any pool member's `model_override`, so once an image pool exists
  a non-admin could send `codex/gpt-image-2` to `/v1/chat/completions`.
  Restrict that clause to members of `Chat`-modality pools. (This is the
  one intentional behavior change on the chat path; test it.)
- `/v1/models` (`proxy/routes.rs:71-96`): exclude image pools unless
  images are enabled. No custom field.

**Runtime state key:** `runtime_key(provider_id, effective_model)` — same
map, distinct keys, so an image cooldown doesn't block chat routing.
Whether image and chat actually share the Codex 5h/weekly quota is
**unverified**; if they do, chat will hit its own 429 and cool
independently, which is still correct.

---

## 4. Request flow — `src/media/images.rs` + `src/media/codex_images.rs`

1. Gate: `images_enabled` setting (off by default; `core::settings`
   bool + `AtomicBool` on `AppState`, like `require_shared_secret`) → 404
   when off. Caller must be authenticated (`Caller.user_id.is_some()`) even
   in open-access mode → 401. Global concurrency semaphore
   (`ROUTER_MEDIA_MAX_CONCURRENCY`, default 4) → 429 when full: each
   request can buffer up to 64 MiB plus JSON copies, so this guards
   memory, not fairness.
2. Buffer body (`proxy::body::buffer_body`), validate per §2.
3. `select_for_modality(.., Modality::Image, ..)`; direct-address check.
4. For each member (skip if runtime-unavailable):
   a. `credentials_for(state, provider)` (already `pub(crate)`, `flow.rs:25`).
   b. Build request (§1) → send with `state.media_http`.
   c. Non-2xx status → classify with the table below.
   d. 401 → `refresh_and_persist_detached(state, provider,
      Arc<dyn ProviderAdapter>, &creds)` (`refresh_lock.rs:30-35`) — build
      the adapter with `adapter_for(provider, state.http.clone())` →
      retry the **same** member once (mirrors chat, `flow.rs:~324-360`).
      Chat and image share the per-provider refresh lock and its re-read
      check (`refresh_lock.rs:58-65`), so concurrent refreshes from both
      paths are safe. Access tokens live ~1h and the background tick runs
      every 6h, so this reactive path is the one that matters.
      Deliberate difference: 9router also refreshes on **403**; we treat
      403 as misconfigured (same as chat).
   e. 2xx → read the SSE body with `read_body_limited` (cap
      `ROUTER_MEDIA_MAX_RESPONSE_BYTES`, default 64 MiB) under an overall
      `tokio::time::timeout` (`ROUTER_MEDIA_REQUEST_TIMEOUT`, default
      300s); parse with `sse_events` (§1).
5. Success (image extracted — **not** merely HTTP 200, unlike chat
   `flow.rs:248-252`) → JSON response, `record_success`, `request_log`
   row (`modality=image`, `units=1`, user_id). Dataset logging: **off**
   for images regardless of toggles.
6. Client disconnect drops the handler future after quota is spent: log
   the `request_log` row from a drop guard so the attempt is still
   counted (same pattern as `proxy/dataset_tee.rs`'s drop guard).

**Classification (Codex images):**

Key fact: Codex is an SSE upstream, so the **HTTP status arrives before
any generation starts** — a non-2xx means no image and no quota spent.
Only failures *after* a 200 are ambiguous.

**Before the stream (HTTP status):**

| Outcome | Fail over? | Runtime state |
|---|---|---|
| Connect/DNS/TLS error | yes | `record_retryable` |
| 429 (incl. `usage_limit_reached` JSON body with `resets_at` / `resets_in_seconds`, per 9router `executors/codex.js:375-395`) | yes | `record_retryable` with cooldown from `retry-after` header **or** `resets_in_seconds` body, capped at 6h (`backoff.rs:45-48` only reads the header — the image path parses the body too) |
| 401 → one refresh → still 401 / refresh fails | yes | `mark_misconfigured` |
| 403 | yes | `mark_misconfigured` |
| 400 whose message says the host model isn't supported for this account/plan (exact text from P0) | yes | `mark_misconfigured` |
| other 400, 413, 422 | **no** → relay | none |
| 5xx, 408, 404, 3xx, other 4xx | yes | `record_retryable` (30s for the "other" bucket, like `flow.rs:487-499`) |

**After a 200 (parsed SSE):**

| Outcome | Fail over? | Runtime state |
|---|---|---|
| `image_generation_call` with `result` | done | `record_success` |
| `response.failed` / `error` with usage/rate-limit code | yes | `record_retryable` (resets_in, capped) |
| `response.failed` / `error` with `moderation_blocked` / invalid-request code, or `image_generation_call.status = failed` | **no** → 400, relay message | none |
| `response.failed` / `error` with unknown code | **no** → 502 | `record_retryable` |
| `response.completed` seen, no image, **`output_text` in a `message` item** (refusal) | **no** → 400 `image_generation_refused` + text | none |
| `response.completed` seen, no image, no `message.output_text` (entitlement, e.g. Free account) | yes | `mark_misconfigured` (5-min re-probe, BL-01) |
| **No** terminal `response.completed` (truncated: read error, idle timeout, cap) | **no** → 502 `upstream_stream_incomplete` | `record_retryable` |
| Overall timeout | **no** → 504 `upstream_timeout` | `record_retryable` |

"Assistant text" means `output_text` inside an output item of type
`message` — **not** reasoning-summary events, which are always present
because we send `reasoning.summary: "auto"`. 9router makes no
refusal/entitlement split at all (any missing image → "may not be
entitled", `imageProviders/codex.js:213-215`); ours is novel and must be
confirmed against P0 dumps. Error codes above are from the Responses API
docs and 9router; exact strings come from P0.

Rationale: after a 200 the account may have spent quota, so only move on
when this account clearly can't do it; a refused prompt must not walk
every account in the pool; a truncated stream must not mark a healthy
account misconfigured.

**HTTP client:** `AppState.media_http` = same builder as
`core::http_client::build_client` (incl. `redirect(Policy::none())`,
SEC-10), `read_timeout = ROUTER_MEDIA_IDLE_TIMEOUT` (default 300s).

**Code sharing:** reuse `codex/transform.rs::sse_events` (make it
`pub(crate)`) instead of a second SSE splitter. No other change to chat
code except the `direct_model_allowed` fix (§3).

**Shutdown:** `ROUTER_DRAIN_TIMEOUT` defaults to 30s, then force-exits
(`main.rs:219-225`) — a deploy kills in-flight image requests whose quota
is already spent. Document: set `ROUTER_DRAIN_TIMEOUT` ≥ the media
timeout on image-serving deployments (or accept the loss). Also document
that a reverse proxy in front of 1router (e.g. nginx default 60s read
timeout) must allow long non-streaming responses.

**Response shape:** `{created, data: [{b64_json, revised_prompt?}],
usage?, size?, quality?, output_format?}` — the optional fields match
current OpenAI Images responses; SDKs tolerate their absence.

---

## 5. Admin & UI

- Pools page: `modality` select on create (`chat` / `image`), immutable
  after (like `wire_format`). Eligible-provider filter
  (`Pools.tsx:486`, currently `provider.wire_format === pool.wire_format`)
  must become `provider.kind === "oauth_codex"` for image pools, or a
  Codex provider with `anthropic` wire is invisible. Image pool member form: Codex providers only,
  model override required, with a datalist of known image models
  (`gpt-image-1.5`, `gpt-image-2`, `gpt-image-2.5`, `gpt-5.5-image`, …) —
  a hint list, not validation beyond the `gpt-image-*` / `*-image` rule.
- Settings: `images_enabled` toggle.
- Stats: `request_log.modality` split (chat vs image count).
- Integration page: a curl example for `/v1/images/generations`.

---

## 6. Phases

**Deploy once at the end (after P1), `images_enabled` off → smoke → on.**

### P0 — Spike against a real account (half a day, throwaway)

**Done 2026-09-25** — see "P0 findings" at the end; they override §1/§2/§4
where they conflict.

- [x] Script (curl or a `#[ignore]` e2e test in `tests/e2e_real_providers.rs`)
      using an existing Codex provider's tokens: send the §1 body, dump the
      raw SSE. Confirm:
      - event names, `item.result`, `revised_prompt` presence,
        `response.completed` shape + `usage`;
      - host model (`gpt-5.5`?) and the exact 400 text when a host model
        isn't allowed for an account;
      - headers: with vs without `codex_cli_rs/0.154.0` + `version` +
        `x-client-request-id`;
      - `response.failed` / `error` shapes; whether usage limits come as
        HTTP 429 JSON or as SSE-embedded errors on a 200;
      - Free-account response; refused-prompt response (is it
        `message.output_text`? a moderation code?);
      - latency, largest gap between events, largest single event size.
      **Adjust §1/§4 from the dumps before P1.** Save sanitized SSE
      fixtures (base64 truncated) to `tests/fixtures/codex_image_*.sse`.

### P1 — Codex images end to end
- [ ] `Config.codex_responses_url` (+ media timeouts, concurrency, host
      model); `tests/common/mod.rs::spawn_app` accepts a `Config`
      override pointing it at wiremock. Enable images in tests through
      the admin settings API (no need for `spawn_app` to return
      `AppState`). A 401→refresh test that still depends on the
      process-global `CODEX_TOKEN_URL` env var goes in its own test
      binary (env var is racy across tests).
- [ ] One `AppState.media: Arc<MediaState>` field (http client,
      `images_enabled` flag, semaphore) to limit churn across ~20
      `AppState {` and ~16 `Config {` literal sites.
- [ ] Migration + `Modality` + `LogEntry` fields + all SQL paths (§3).
- [ ] `select_for_modality`, `select()` guard, `upsert_member` rules,
      `direct_model_allowed` chat-pool-only fix, `/v1/models` filter.
- [ ] `src/media/{mod,images,codex_images}.rs`, route merged under
      `require_bearer` in `app.rs`.
- [ ] Admin API + UI (§5).
- [ ] Tests (wiremock + recorded fixtures):
      happy path returns `b64_json` + `revised_prompt`; request body sent
      upstream matches §1 for both model families (fresh
      `prompt_cache_key`, no `reasoning` key for `-image`); `n=2` / `url` /
      `stream:true` / empty prompt / bad `size`/`quality` enum → 400;
      unknown image model → 400;
      429 with `resets_in_seconds` body → next account + cooldown from
      body; SSE usage-limit error → next account;
      5xx / 404 before stream → next account;
      `response.completed` + no image + no text → next account +
      misconfigured; refusal `message.output_text` → 400, **no** second
      account called; moderation code → 400, no failover; stream cut
      before `response.completed` → 502, **not** misconfigured, no
      failover; image event > 16 MiB parses; 401 → refresh → retry same
      account; slow stream beyond chat idle timeout succeeds; response cap
      trips; concurrency cap → 429; anonymous in open-access → 401;
      disabled → 404; non-admin direct `codex/gpt-image-2` → 400;
      non-admin `/v1/chat/completions` with an image-pool member's model
      via direct addressing → 400 (chat-leak fix); `upsert_member` rejects
      non-Codex / missing override in image pools;
      chat route with image pool id → 400; image route with chat pool → 400;
      rejected selection doesn't advance round-robin; export → import keeps
      `modality`, `strategy`, `sticky_limit`; old export imports as chat;
      request_log row has `modality=image`, `units=1`; chat test suite
      unchanged and green.
- [ ] Frontend: modality select, Codex-kind provider filter for image
      pools (`Pools.tsx:486`), model datalist, settings toggle; tests.
- [ ] Docs: ARCHITECTURE.md section + README env vars, drain-timeout and
      reverse-proxy timeout notes.

### P2 — OpenAI-compatible passthrough images (API-key providers)
- `media_api` column, streamed passthrough to derived
  `/images/generations` URL (reviewed design in the media plan §5.1/§5.5).

### P3 — Later
- ~~`/v1/images/edits` (multipart, OpenAI-official) → Codex `input_image` +
  `action: "edit"`~~ — done on `feature/image-edits` (masks not supported);
  OpenAI-shaped streaming (`partial_images`); `n>1` by
  sequential calls with explicit opt-in; Gemini/Antigravity image adapters.

---

## 7. Open questions

- **Q1 (settled):** host model = `Config` default `gpt-5.5`
  (`ROUTER_CODEX_IMAGE_HOST_MODEL`), confirmed in P0. Not
  `provider.upstream_model` — that couples chat config to image behavior,
  and mini/spark/review models may lack the tool.
- **Q2 (settled):** direct addressing admin-only + `OauthCodex` kind check.
- **Q3 (settled):** no per-user limit in v1; global concurrency cap only.

## 8. Risk note

Using ChatGPT subscription accounts through the Codex backend for images is
the same ToS/ban exposure 1router already takes for Codex chat, amplified by
volume: image generation drains the 5-hour/weekly Codex quota fast. 9router
marks its Codex provider `deprecated` with a risk notice. Keep
`images_enabled` off by default and consider dedicated accounts for image
pools.

---

## Review revisions (Opus review, 2026-09-25)

Blockers fixed:
1. Codex `/responses` URL was a private const, un-mockable → `Config.codex_responses_url` (§1, P1).
2. "No failover on pre-stream 5xx" was carried over from the non-streaming
   media plan; wrong for an SSE upstream where the status precedes any work →
   split pre-stream vs post-200 tables (§4).
3. "No image + no text → entitlement" would misfire on truncated streams
   (incl. the 16 MiB `SseFramer` cap) → require `response.completed`; parse
   from `read_body_limited` + `sse_events` (§1, §4).
4. Member rule belongs in `queries::upsert_member`, not a non-existent
   `queries::put_member`; keep selection-time skip for import (§3).

Should-fix applied: image-specific error extractor (3 shapes + 429 body
`resets_in_seconds`, capped); precise refusal definition
(`message.output_text`, not reasoning summary); refresh wiring via
`adapter_for` + shared lock, 403 difference called out; chat-side
`direct_model_allowed` leak fixed + `OauthCodex` kind check; fresh
`prompt_cache_key`, omit `reasoning` for `-image`; client-version headers
tested in P0; drain-timeout / reverse-proxy docs + drop-guard logging;
global concurrency semaphore; account-specific "model not supported" 400 →
failover; `Pools.tsx` provider filter by kind; enum validation of tool
fields.

Nits applied: `AppState.media: Arc<MediaState>`; `record_success` only
after image extracted; optional `usage`/`size`/`quality`/`output_format`
in response; don't make `spawn_app` return `AppState`; removed
separate-quota claim (unverified).

Still unverified — P0 must settle: real error event shapes; 429 vs
SSE-embedded usage limits; Free-account and refusal responses;
`revised_prompt` presence; version-header gating; event gaps / latency
(is 300s right); largest single-event size.

---

## P0 findings (real account, 2026-09-25)

One ChatGPT **Plus** account (Codex CLI login), 6 calls: `happy`,
`badhost`, `badsize`, `badtool`, `refuse`, `alias` (`-image` family). 5
generated an image; the 5h quota window went 0% → 1%, weekly stayed 11%.
Python script + sanitized dumps live in session scratchpad; the P1 fixtures
are rebuilt from them with identifiers stripped (`x-codex-turn-state`
header, `safety_identifier`, `ig_`/`msg_`/`resp_` ids, account id).

**Confirmed as planned:**
- 1router's existing headers work (`codex_cli_rs/0.3.12` UA, no `version`
  / `x-client-request-id`). → **Drop** `Config.codex_client_version`.
- Host model `gpt-5.5` works for `gpt-image-*` and for the `gpt-5.5-image`
  alias (`tool_choice: "auto"`, no `reasoning`, no tool model).
- Event sequence: `response.created` → `in_progress` →
  `output_item.added` (image_generation_call) →
  `image_generation_call.{in_progress,generating,completed}` →
  `output_item.done` (image, `item.result` base64 + `revised_prompt` +
  actual `size`/`quality`/`background`/`output_format`) →
  `output_item.added`/`done` (message) → `response.completed`.
- `response.completed.response.output` is `[]` — the image is **not**
  repeated there. Largest event ≈ 2.2 MB (a 1254² PNG); 64 MiB cap is
  ample.
- Latency 20–55s; longest gap between events ≈ 30s, and upstream emits a
  `keepalive` event (`{"type":"keepalive"}`) during long gaps. 300s
  overall / chat idle 120s are safe. The parser must ignore `keepalive`.
- Image usage is in `response.completed.response.tool_usage.image_gen`
  (`input_tokens`, `output_tokens`, `total_tokens`, with `*_details`) —
  **not** in `response.usage` (that's the host model's text tokens). Use
  `tool_usage.image_gen` for our `usage` field.
- Unsupported host model → **HTTP 400 before any stream**, body
  `{"detail":"The '<model>' model is not supported when using Codex with a ChatGPT account."}`.
  Note the shape is `detail`, not `error`. Match on
  `"model is not supported when using Codex"` → `mark_misconfigured` +
  fail over (§4 row). No quota spent.

**Contradicts the plan — change P1 accordingly:**
1. **Tool parameters are not honored.** Upstream normalizes the tool to
   `{model: "gpt-image-2-codex", size: "auto", quality: "auto", ...}`
   regardless of what we send (visible in `response.completed.tools`).
   `size: "1024x1024"` → 1254×1254; `size: "7x7"` and
   `model: "gpt-image-nope"` → **no error**, an image is generated anyway
   (quota spent). The host model reads the prompt and picks
   size/quality itself.
   - §2: keep validating `size`/`quality`/`background`/`output_format`
     (cheap, catches client bugs) but document them as **hints**. Also
     append a short hint to the prompt text (e.g. "Output size: 1024x1024,
     quality: low") so the host model has a chance to follow them —
     verify in P1 smoke, don't promise it.
   - Response: always return the **actual** `size`, `quality`,
     `background`, `output_format` from the `image_generation_call` item,
     so clients see what they really got.
   - Tool `model` from the client is meaningless: any `gpt-image-*` client
     model maps to the same upstream. Keep accepting `gpt-image-*` names
     (SDK compatibility), send `gpt-image-2`, and don't pretend other
     names select a different model.
2. **Text comes with the image.** Every response has an assistant
   `message` next to the image — usually `""`, sometimes "Here's a cat!".
   → Having text **never** means refusal when an image exists. Refusal =
   `response.completed` + no image + **non-empty** `output_text`.
3. **Refusals are soft.** The copyright prompt ("pixel-perfect official
   Mickey Mouse") was **not** refused: the host model rewrote the prompt
   ("an original cartoon mouse ... Do not copy Disney"), generated that,
   and explained in the message ("Sorry, I can't reproduce ... I created
   an original ... instead"). Quota spent, HTTP 200, `status: completed`,
   no moderation field.
   - The user gets a *different* image than asked for and only
     `revised_prompt` hints at it. → Add a non-standard optional field
     `data[0].text` carrying non-empty assistant text (OpenAI SDKs ignore
     unknown fields). Document it.
   - The no-image + text branch (hard refusal) was **not** observed; keep
     it in §4 (400 `image_generation_refused`, no failover) as the
     defensive path, tested by fixture only. Deliberately not probed with
     harmful prompts.

**Still unverified (can't be produced with one Plus account):**
Free-account (entitlement) response; usage-limit exhaustion (429 JSON vs
SSE error); `response.failed` / `event: error` shapes; 401 on an expired
token. P1 covers these with wiremock fixtures built from 9router's
handling and the Responses API docs; the §4 table stays as written.
Treat the first production occurrence of each as a fixture-update task
(log the raw error body at `warn`, truncated, without tokens).
