# Gemini API provider — Design

## Goal

Add Google's Gemini Generative Language API (`generateContent` /
`streamGenerateContent`) as a first-class preset provider — a new
`ProviderKind::Gemini`, API-key authenticated (no OAuth, no browser login),
served by a bespoke adapter that translates 1router's internal OpenAI
Chat-Completions shape to/from Gemini's native request/response/streaming
shapes. Once done, a client hitting either `/v1/chat/completions` or
`/v1/messages` can be routed to a real Gemini model with no shape mismatch,
the same guarantee `passthrough`, Codex, and Command Code providers already
give.

This is explicitly about Gemini's **native** wire format
(`contents`/`parts`/`candidates`, its own tool-call/function-call encoding,
its own SSE shape), not the OpenAI-compatibility shim Google also publishes
at `/v1beta/openai/chat/completions`. That shim needs zero translation and
already works today as an ordinary `passthrough` provider with
`wire_format: openai` — it is *not* the reason this feature exists. Anyone
who only wants that shim doesn't need this plan; this plan is for people who
want Gemini-specific behavior the shim doesn't fully expose (Google's own
`thinkingConfig`, `safetySettings`, native `functionDeclarations`/
`functionCall`/`functionResponse` shapes, exact `usageMetadata` fields), or
who want Gemini available without depending on Google's compatibility layer
staying maintained.

## Why a bespoke adapter, not a `passthrough` translation

`HttpAdapter` (`src/providers/adapter/http.rs`, `ProviderKind::Passthrough`)
already translates bidirectionally, but only between **OpenAI and Anthropic**
Chat/Messages shapes via `claude_bridge`. Gemini's request shape
(`{"contents":[{"role":"user"|"model","parts":[...]}],
"systemInstruction":{...}, "generationConfig":{...},
"tools":[{"functionDeclarations":[...]}]}`), its response shape
(`{"candidates":[{"content":{"parts":[...]},"finishReason":...}],
"usageMetadata":{...}}`), its endpoint path convention (model name and
call-shape live in the URL: `.../models/{model}:generateContent` vs.
`.../models/{model}:streamGenerateContent?alt=sse`), and its auth convention
(`x-goog-api-key` header, not `Authorization: Bearer` or `x-api-key`) are all
different enough from both existing shapes that folding them into
`claude_bridge` would turn a 2-format bridge into a 3-format one throughout —
every call site would need a 3-way match instead of 2. `codex` and
`commandcode` already establish the precedent for this: a **bespoke
adapter module that normalizes its provider's native protocol down to the
one shape everything else already understands (OpenAI Chat Completions)**,
then hands off to `claude_bridge` unchanged for the Anthropic side. Gemini
follows the same shape.

## Reuse, not new machinery

- `src/providers/adapter/codex/claude_bridge.rs`'s existing
  `claude_to_openai_request` / `convert_openai_sse_to_claude_sse` /
  `openai_json_to_claude_message` (and their reverse-direction siblings from
  the passthrough-translation phase) are reused **verbatim**, exactly as
  `commandcode::adapter` reuses them today. Gemini's own module only ever
  has to reason about Gemini ⇄ OpenAI; OpenAI ⇄ Anthropic is already solved.
- `claude_bridge::reframe_sse_blocks` (format-agnostic byte reframing) is
  reused verbatim to turn Gemini's raw `bytes_stream()` chunks into complete
  `data: {...}\n\n` blocks before parsing — Gemini's `alt=sse` stream uses
  plain `data: {json}\n\n` framing with no `event:` line, the same shape
  `HttpAdapter` already has to reframe for a plain passthrough provider.
- `proxy::backoff::classify` is reused verbatim for `classify_error` — no
  bespoke retry logic, matching the explicit rule already established for
  Codex and Command Code (`proxy::flow`'s failover owns retries, not the
  adapter).
- The provider row schema needs no migration: `providers.kind` is
  `TEXT NOT NULL DEFAULT 'passthrough'` with no `CHECK` constraint
  (`migrations/0001_init.sql:5`), so a new `ProviderKind` variant is a pure
  Rust-side change. Credentials are the plain `providers.api_key` column,
  exactly like `Passthrough` — Gemini needs no OAuth/refresh machinery at
  all, unlike Codex/Command Code, so `refresh_task.rs` and
  `proxy/flow.rs`'s `AuthExpired` recovery branch need no changes (they
  already only special-case `OauthCodex`).

## What's new: `src/providers/adapter/gemini/`

Mirrors `commandcode/`'s breakdown minus OAuth/browser-login entirely (no
`oauth.rs`, `refresh.rs`, or `browser_login.rs` — there is no token exchange):

- `mod.rs` — re-exports.
- `transform.rs` — pure functions, the testable half:
  - `openai_request_to_gemini(openai_json: &Value) -> Value` — request
    translation (see below).
  - `gemini_response_to_openai_json(gemini_json: &Value, model: &str) -> Value`
    — non-streaming response translation.
  - `GeminiStreamState` + `gemini_chunk_to_openai_chunk(&mut GeminiStreamState, chunk: &Value, model: &str) -> Option<Value>`
    — one Gemini streamed `GenerateContentResponse` → one OpenAI
    `chat.completion.chunk`, or `None` for an empty/keepalive chunk.
  - `convert_gemini_sse_to_openai_sse<S, E>(upstream: S, model: String) -> impl Stream<Item = Result<Bytes, E>>`
    — consumes **already-reframed** `data: {...}\n\n` blocks (caller runs
    `reframe_sse_blocks` first, same contract `HttpAdapter` already follows),
    emits framed OpenAI `chat.completion.chunk` SSE, terminated by
    `data: [DONE]\n\n` when the upstream stream ends (Gemini has no
    `[DONE]` marker of its own — same asymmetry `convert_claude_sse_to_openai_sse`
    already handles for Anthropic).
  - `gemini_embedded_error(body: &Value) -> Option<String>` — Gemini reports
    errors as a top-level `{"error":{"message":...,"code":...}}` object even
    on some 200 responses in streaming mode; surfaced the same way
    `commandcode::transform::ndjson_embedded_error` is.
- `adapter.rs` — `GeminiAdapter`, wiring the above into `ProviderAdapter`.

### Request translation (`openai_request_to_gemini`)

Given the internal OpenAI-shape body (already bridged from Anthropic first
via `claude_bridge::claude_to_openai_request` if `client_wire == Anthropic`,
exactly like `HttpAdapter`/`CodexAdapter`/`CommandCodeAdapter` all do before
running their own provider-specific transform):

| OpenAI shape | Gemini shape |
|---|---|
| `messages[].role == "system"` (usually the first message) | lifted out of `contents`, becomes `systemInstruction: {"parts":[{"text": ...}]}` |
| `messages[].role == "user"` | `contents[].role == "user"` |
| `messages[].role == "assistant"` | `contents[].role == "model"` (Gemini has no "assistant" role) |
| `messages[].role == "tool"` (a tool result, keyed by `tool_call_id`) | a `user`-role `contents[]` entry whose `parts` is `[{"functionResponse":{"name":<looked up from the matching prior tool_call's function.name>,"response":{"content": <parsed-or-wrapped JSON>}}}]` — Gemini has no separate "tool" role, function responses ride inside a user turn |
| `messages[].content` (string) | `parts: [{"text": content}]` |
| `messages[].content` (array, image/text blocks — OpenAI `image_url` blocks) | `parts` gains `{"inlineData":{"mimeType":...,"data":<base64>}}` for a data-URL image, or is dropped with a documented limitation for a remote `http(s)://` image URL (Gemini's `inlineData` needs bytes, not a fetchable URL — out of scope to add a fetch-and-inline step in v1) |
| `messages[].tool_calls[]` (assistant function call) | `parts` gains `{"functionCall":{"name":...,"args": <parsed JSON object from the OpenAI `arguments` string>}}}` |
| `tools[].function.{name,description,parameters}` | `tools: [{"functionDeclarations":[{"name","description","parameters"}]}]` (JSON Schema `parameters` carried through as-is — Gemini's schema subset is close enough to OpenAI's that no filtering is done in v1; an unsupported keyword is Gemini's problem to 400 on, not this layer's to pre-validate) |
| `tool_choice` | `toolConfig: {"functionCallingConfig":{"mode": "AUTO"\|"ANY"\|"NONE"}}` — `"auto"`→`AUTO`, `"none"`→`NONE`, a named-function object or `"required"`→`ANY` (Gemini's `ANY` has no per-name pin the way OpenAI's named-tool-choice does; this is a known lossy edge, documented, not solved) |
| `temperature`, `top_p`, `max_tokens`, `stop`/`stop_sequences`, `n` | `generationConfig: {"temperature","topP","maxOutputTokens","stopSequences","candidateCount"}` |
| `stream` | dropped from the body — Gemini encodes streaming in the **URL** (`:generateContent` vs `:streamGenerateContent?alt=sse`), not a body field. `GeminiAdapter::build_request` reads `client_body`'s `stream` field once to choose the URL, the same way it must already inspect the body to know which endpoint to hit. |
| `model` | dropped from the body entirely — becomes the `{model}` URL path segment, always `provider.upstream_model` (never the client's pool-id `model`, matching every other adapter's rewrite-to-upstream-model behavior) |

### Response translation (`gemini_response_to_openai_json`, non-streaming)

| Gemini shape | OpenAI shape |
|---|---|
| `candidates[0].content.parts[]` where a part has `"text"` | concatenated into `choices[0].message.content` |
| `candidates[0].content.parts[]` where a part has `"functionCall"` | `choices[0].message.tool_calls[] = [{"id": <synthesized, e.g. "call_" + a stable counter — Gemini assigns no call id of its own>, "type":"function","function":{"name":...,"arguments": <serialized JSON string of `args`>}}]` |
| `candidates[0].finishReason` | `choices[0].finish_reason`: `"STOP"`→`"stop"`, `"MAX_TOKENS"`→`"length"`, a function-call-bearing response→`"tool_calls"` (Gemini's own `finishReason` stays `"STOP"` even when it called a function, so this is a `parts`-presence check, not a `finishReason` string match — document this explicitly, it's the one place Gemini's signal genuinely differs in *kind*, not just spelling), `"SAFETY"`/`"RECITATION"`/`"PROHIBITED_CONTENT"`→`"content_filter"`, anything else→`"stop"` |
| `usageMetadata.{promptTokenCount,candidatesTokenCount,totalTokenCount}` | `usage.{prompt_tokens,completion_tokens,total_tokens}` (Gemini also reports `cachedContentTokenCount` when context caching is used — map to `usage.prompt_tokens_details.cached_tokens`, mirroring how `commandcode::transform` already maps its own cache fields) |
| top-level `error.message` | not a 200 in practice for this path — `GeminiAdapter` treats any non-2xx as today's adapters do (status passed through, `classify_error` decides retry eligibility) |

### Streaming translation

Gemini's `streamGenerateContent?alt=sse` sends one `data: {...}\n\n` block
per emitted chunk, each a **complete `GenerateContentResponse`** whose
`candidates[0].content.parts[].text` is the *incremental* text for that
chunk (Gemini does not resend prior text — this is delta-shaped like
OpenAI's stream, not cumulative-shaped like some other APIs, which is why a
straightforward per-chunk translation works without needing to diff against
previous state for the text case). Function-call parts, however, tend to
arrive whole in one chunk rather than incrementally, and the final chunk
carries `finishReason` + `usageMetadata`. `GeminiStreamState` tracks a
stable synthesized `id`/`created` (generated once, reused across the whole
stream — same idea as `commandcode::transform::ChunkState` and
`codex::transform::SseChunkState`) plus whatever running function-call-id
counter is needed for the arguments-as-string re-encoding above.

### Endpoint, auth, and models

- `GEMINI_API_ROOT`: `https://generativelanguage.googleapis.com` (fixed
  constant, like Command Code's `GENERATE_URL` — no `base_url` field is
  used or stored for this kind; Vertex AI's project/location/IAM-based
  variant is a materially different auth model and is explicitly out of
  scope for v1, see below).
- Non-streaming: `POST {GEMINI_API_ROOT}/v1beta/models/{upstream_model}:generateContent`
- Streaming: `POST {GEMINI_API_ROOT}/v1beta/models/{upstream_model}:streamGenerateContent?alt=sse`
- Auth: `x-goog-api-key: {creds.api_key}` header (Google's documented
  alternative to `?key=` query param — kept out of the URL so it never
  lands in access logs or `reqwest::Request::url()` debug output, matching
  the spirit of every other adapter's choice of header over query-string
  auth).
- Model discovery: `GET {GEMINI_API_ROOT}/v1beta/models?key={api_key}` (the
  list endpoint takes `key` as a query param regardless — no header
  alternative documented) returns
  `{"models":[{"name":"models/gemini-2.0-flash","supportedGenerationMethods":[...]}]}`;
  a small `fetch_gemini_models` sibling to `fetch_commandcode_models` strips
  the `"models/"` prefix and keeps only entries whose
  `supportedGenerationMethods` includes `"generateContent"` (the same list
  otherwise includes embedding-only and other non-chat model rows that
  would 400 if picked).

## What changes elsewhere

- `src/core/model.rs`: add `ProviderKind::Gemini` (serializes/stores as
  `"gemini"`). No `supports_wire`-equivalent gate exists anymore to widen —
  that mechanism was deleted in the universal-passthrough-translation phase
  (`docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`);
  every kind already supports every wire format unconditionally, so this is
  the one place a brand-new kind is now *less* work to add than Command Code
  was.
- `src/providers/adapter/mod.rs`: `pub mod gemini;` + one `adapter_for_wire`
  arm.
- `src/providers/routes.rs`: `discover_and_cache_models`/
  `spawn_bounded_discovery` gain a `ProviderKind::Gemini =>
  fetch_gemini_models(...)` arm alongside Passthrough/CommandCode's. This
  kind is **not** added to `is_oauth_kind` — it behaves like `Passthrough`
  there (`credential_configured` is `p.api_key.is_some()`, not an
  oauth-state lookup), which is already the default fallthrough for any
  kind not explicitly OAuth, so no code changes are needed at that
  particular call site, only at the discovery match (which is exhaustive
  over `ProviderKind` and will fail to compile until updated).
- `src/onboarding.rs`: a fourth `Select` item + `add_gemini_provider(db,
  http) -> anyhow::Result<Provider>` next to `add_codex_provider`/
  `add_commandcode_provider`, but shaped like a *simpler* `add_passthrough_provider`
  (prompt name, prompt API key, discover-then-pick model, no wire-format
  prompt needed up front the way Passthrough needs one — Gemini's own
  `wire_format` field is written but never read by the adapter, since
  translation direction is decided by `client_wire` at request time exactly
  like Codex/Command Code; default it to `OpenAi` for cosmetic/pool-listing
  consistency).
- `src/providers/oauth_routes.rs` / admin UI: **no new endpoint** — unlike
  Command Code, there is no separate "paste the key" panel needed, because
  Gemini's credential *is* `providers.api_key`, the same field the ordinary
  provider-create/edit form already has for `Passthrough`. The only admin UI
  change is `frontend/src/pages/Providers.tsx`'s kind `<select>` gaining a
  `<option value="gemini">Gemini (API key)</option>` and the create/edit
  form treating `kind === "gemini"` the way it already treats
  `kind === "passthrough"` for the API-key field, while suppressing the
  `base_url`/wire-format-of-upstream fields Passthrough needs but Gemini's
  fixed-endpoint adapter does not. `Settings.tsx`'s `discoverableProviders`
  filter gains `|| p.kind === "gemini"`.
- `README.md`: Gemini joins the provider list; a short "Wire formats" note
  that Gemini, like Codex/Command Code, translates rather than requiring a
  matching `wire_format`.

## Explicitly out of scope

- **Vertex AI's Gemini endpoint** (project/location-scoped URLs, IAM/OAuth
  service-account auth instead of a flat API key) — a different enough auth
  and addressing model that it would be its own `ProviderKind`, not a
  `base_url` override of this one. Not attempted here.
- **Multimodal input beyond inline base64 images** — video, audio, and
  remote-URL image `inlineData` (which needs a fetch-and-base64 step this
  phase does not add) are not translated; a request containing one either
  drops the unsupported part (documented lossy behavior, consistent with
  how tool_choice's `ANY`-vs-named gap is handled) or is left for Gemini
  itself to reject.
- **`thinkingConfig`/extended-thinking passthrough** — Anthropic
  "thinking" blocks and Gemini's own `thinkingConfig`/thought-summary parts
  are not modeled in either direction, matching the exact same
  already-accepted gap in the Codex/Command Code direction
  (`2026-08-04-universal-passthrough-translation-design.md`'s "Explicitly
  out of scope" section).
- **`safetySettings` passthrough** from any client-supplied field — no
  OpenAI or Anthropic client field maps to it; a fixed, permissive default
  (or Gemini's own API default) is used, not configurable in v1.
- **Context caching (`cachedContent`)** as a request feature — only the
  *reporting* side (`cachedContentTokenCount` → `usage.prompt_tokens_details.cached_tokens`)
  is in scope; nothing in this phase lets a client request cache creation
  or reuse.
- No schema/migration changes, no new admin endpoints beyond the kind
  dropdown addition (contrast with Command Code, which needed a
  `POST .../commandcode/key` endpoint precisely because it has no plain
  `api_key` field to reuse — Gemini does, so it needs none).
