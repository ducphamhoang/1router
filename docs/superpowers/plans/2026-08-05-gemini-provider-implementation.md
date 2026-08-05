# Gemini API Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Google's Gemini Generative Language API as a first-class
preset provider — a new `ProviderKind::Gemini` served by a bespoke adapter
that translates the OpenAI Chat-Completions shape 1router speaks internally
to/from Gemini's native `generateContent`/`streamGenerateContent` protocol,
API-key authenticated (no OAuth, no browser login, no new admin endpoint).

**Architecture:** One new adapter module,
`src/providers/adapter/gemini/` (`adapter.rs`, `transform.rs`, `mod.rs`),
mirroring `src/providers/adapter/commandcode/`'s breakdown minus
`oauth.rs`/`refresh.rs`/`browser_login.rs` entirely (no token exchange, no
refresh, no local callback listener — this kind authenticates with a flat
API key stored in the existing `providers.api_key` column, the same field
`Passthrough` already uses). Everything else is gate-widening: one new
`ProviderKind` variant, one new `adapter_for_wire` arm, one discovery
branch, one wizard branch, one admin dropdown option + conditional form
fields. The Anthropic wire is served by reusing `codex::claude_bridge`
verbatim in both directions, exactly as Command Code already does.

**Tech Stack:** Existing deps only — `reqwest`, `serde_json`, `futures`,
`bytes`, `chrono`, `dialoguer`. **No new crate is added.**

Design spec: `docs/superpowers/specs/2026-08-05-gemini-provider-design.md`
— read it in full before starting; it has the exact field-by-field
translation tables this plan implements.

## Global Constraints

- Package is `router`, binary is `1router`; import via `use router::...`.
  Build/test with `cargo build --offline` / `cargo test --offline`.
- **No new Cargo dependency, no migration.** `providers.kind` is
  `TEXT NOT NULL DEFAULT 'passthrough'` with no `CHECK` constraint
  (`migrations/0001_init.sql:5`), so `ProviderKind::Gemini` needs no schema
  change. Credentials go in the existing `providers.api_key` column — no
  `provider_oauth_state` row is ever written for this kind.
- DB/JSON value of the new kind is exactly `gemini` (snake_case via the
  existing `#[sqlx(rename_all = "snake_case")]` / `#[serde(rename_all =
  "snake_case")]` on `ProviderKind` — for a single-word variant this is
  just the lowercased name, no underscores to get wrong).
- Fixed upstream constants, used verbatim:
  - API root: `https://generativelanguage.googleapis.com`
  - non-streaming: `{root}/v1beta/models/{upstream_model}:generateContent`
  - streaming: `{root}/v1beta/models/{upstream_model}:streamGenerateContent?alt=sse`
  - models list: `{root}/v1beta/models?key={api_key}`
- Auth is `x-goog-api-key: {creds.api_key}` on the generate/stream calls;
  the models-list call uses `?key=` (no header alternative for that one
  endpoint per Google's docs) — do not use `Authorization: Bearer` or
  `x-api-key`, Gemini accepts neither.
- There is **no `supports_wire`/kind-gate to widen** — that mechanism was
  deleted in the universal-passthrough-translation phase
  (`docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`).
  Every `ProviderKind` already supports every client wire format
  unconditionally. Do not go looking for a gate to edit in
  `pools/routes.rs` or `pools/select.rs` — there isn't one anymore.
- `refresh_task.rs`'s loop and `proxy/flow.rs`'s `AuthExpired` recovery
  branch are `ProviderKind::OauthCodex`-only already and need **no** change
  — this kind has no refresh concept at all (`needs_refresh` is always
  `false`, matching `HttpAdapter`).
- `is_oauth_kind` (`src/providers/routes.rs`) stays
  `matches!(kind, ProviderKind::OauthCodex | ProviderKind::OauthCommandCode)`
  — do **not** add `Gemini` to it. It must fall through to the
  `p.api_key.is_some()` branch, the same as `Passthrough`.
- axum is pinned to **0.7**: any new route uses `:id`, never `{id}` — this
  plan adds no new route, but double-check if that changes.
- Tests that bind a real `TcpListener` (anything using
  `tests/common::spawn_app`) are **BLOCKED in a Codex sandbox**
  (`TcpListener::bind` → `PermissionDenied`). Re-run those specific tests
  outside the sandbox before calling a task done. This plan has no
  browser-login listener of its own (unlike Command Code), so the *only*
  sandbox-blocked tests here are the ordinary `spawn_app`-based integration
  tests in Tasks 4, 6, and 7.
- With the `ui` feature default-on, any `cargo build/test --offline`
  dispatched into a Codex worktree must add `--no-default-features`.
- Out of scope (v1, see design spec for full rationale): Vertex AI's
  Gemini endpoint, remote-URL image fetching for `inlineData`, `thinking`/
  `thinkingConfig` passthrough, `safetySettings` passthrough, requesting
  context-cache creation (only *reporting* `cachedContentTokenCount` is in
  scope).

---

### Task 1: `ProviderKind::Gemini`

Introduce the variant. Unlike Command Code, there is no `supports_wire`
equivalent left to widen (already deleted) and no `update_provider`
wire-format-flip guard to touch (that guard only special-cases the two
OAuth kinds, and Gemini isn't one — flipping a Gemini provider's
`wire_format` field is already unguarded, matching `Passthrough`... except
`Passthrough`'s flip *is* guarded by pool-homogeneity elsewhere via
`wire_format == w`? No: re-read `src/providers/queries.rs::update_provider`
before writing this task's diff — confirm exactly which providers that
guard still applies to now that `supports_wire` is gone, since the design
spec's claim ("Gemini's `wire_format` is written but never read by the
adapter") means a flip should be as unguarded for Gemini as it already is
for Codex/CommandCode. If the guard's condition still reads
`p.kind != ProviderKind::OauthCodex` unchanged since the CommandCode plan
widened it to `!matches!(p.kind, OauthCodex | OauthCommandCode)`, decide
whether Gemini needs adding to that list too and record the reasoning in
the commit — do not silently skip this check.

**Files:**
- Modify: `src/core/model.rs` (`ProviderKind` enum; tests at the bottom)
- Test: `cargo test --offline --lib core::model`

**Interfaces:**
- Consumes: nothing.
- Produces: `ProviderKind::Gemini` (serializes to `"gemini"`). Tasks 2-6 all
  match on this variant.

- [ ] **Step 1: Write the failing test**

In `src/core/model.rs`'s `mod tests`, add
`gemini_kind_serializes_as_gemini` — `serde_json::to_string(&ProviderKind::Gemini)`
== `"\"gemini\""`, and the round-trip back via `serde_json::from_str`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib core::model`
Expected: FAIL to compile — `no variant named 'Gemini' found for enum 'ProviderKind'`.

- [ ] **Step 3: Add the variant**

Add `Gemini` to `ProviderKind`. Fix any resulting non-exhaustive-match
compile errors *only* where the compiler demands it
(`providers/adapter/mod.rs`'s `adapter_for_wire` and
`providers/routes.rs`'s `discover_and_cache_models` are handled in Tasks 3
and 4 respectively — a `todo!()` placeholder there is acceptable until
then, but must be gone by the end of those tasks).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib` — expected PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/model.rs
git commit -m "feat(providers): add ProviderKind::Gemini

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: `gemini::transform` — request/response/stream translation

The pure-function half of the adapter, testable with zero network. This is
the bulk of the work — every row in the design spec's translation tables
becomes an assertion here.

**Files:**
- Create: `src/providers/adapter/gemini/mod.rs`
- Create: `src/providers/adapter/gemini/transform.rs`
- Modify: `src/providers/adapter/mod.rs` (add `pub mod gemini;` — the
  `adapter_for_wire` arm lands in Task 3)
- Test: `cargo test --offline --lib providers::adapter::gemini::transform`

**Interfaces:**
- Consumes: nothing from Task 1 directly (pure JSON-in/JSON-out functions).
- Produces, for Task 3:
  - `pub fn openai_request_to_gemini(body: &Value) -> Value`
  - `pub fn gemini_response_to_openai_json(value: &Value, model: &str) -> Value`
  - `pub struct GeminiStreamState` + `pub fn new() -> Self`
  - `pub fn gemini_chunk_to_openai_chunk(state: &mut GeminiStreamState, chunk: &Value, model: &str) -> Option<Value>`
  - `pub fn convert_gemini_sse_to_openai_sse<S, E>(upstream: S, model: String) -> impl Stream<Item = Result<Bytes, E>>`
    (consumes **already-reframed** blocks — caller runs
    `claude_bridge::reframe_sse_blocks` first, same contract `HttpAdapter`
    follows)
  - `pub fn gemini_embedded_error(body: &Value) -> Option<String>`

- [ ] **Step 1: Write the failing tests**

Create `transform.rs` with only a `#[cfg(test)] mod tests` and stub
signatures. Tests to write (all `#[test]`, no async needed except the
stream ones which use `#[tokio::test]` + `futures::stream::iter`):

1. `openai_request_to_gemini_maps_roles_and_text` — given
   `{"messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"}],"max_tokens":100,"temperature":0.5}`,
   assert `systemInstruction == {"parts":[{"text":"be terse"}]}`,
   `contents[0] == {"role":"user","parts":[{"text":"hi"}]}` (the system
   message is *not* also present in `contents`), and
   `generationConfig == {"temperature":0.5,"maxOutputTokens":100}`.
2. `openai_request_to_gemini_maps_assistant_to_model_role` — an
   `{"role":"assistant","content":"ok"}` message becomes
   `{"role":"model","parts":[{"text":"ok"}]}`.
3. `openai_request_to_gemini_converts_tools_and_tool_calls` — OpenAI
   `tools:[{type:"function",function:{name,description,parameters}}]`
   becomes `tools:[{"functionDeclarations":[{name,description,parameters}]}]`;
   an assistant message with
   `tool_calls:[{id:"c1",type:"function",function:{name:"get_weather",arguments:"{\"city\":\"nyc\"}"}}]`
   becomes a `model`-role content whose `parts` includes
   `{"functionCall":{"name":"get_weather","args":{"city":"nyc"}}}` (note:
   `args` is a real JSON object, not a re-escaped string — the OpenAI
   `arguments` string is parsed, not carried through raw); a
   `{"role":"tool","tool_call_id":"c1","content":"72F"}` message becomes a
   `user`-role content whose `parts` is
   `[{"functionResponse":{"name":"get_weather","response":{"content":"72F"}}}]`
   (the `name` is recovered from the earlier matching `tool_call_id`, not
   present on the tool message itself — assert this lookup works across
   the two messages in sequence).
4. `openai_request_to_gemini_maps_tool_choice` —
   `tool_choice:"none"`→`toolConfig.functionCallingConfig.mode == "NONE"`;
   `"auto"`→`"AUTO"`; `"required"` or a named-function object→`"ANY"`; absent
   `tool_choice` → no `toolConfig` key at all.
5. `openai_request_to_gemini_drops_stream_and_model_from_body` — a body
   with `"stream":true,"model":"pool-x"` produces a Gemini body with
   neither key present anywhere (model addressing is the caller's job via
   the URL, per the design spec).
6. `gemini_response_to_openai_json_maps_text_and_usage` —
   `{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"},],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13}}`
   → `choices[0].message.content == "hello"`,
   `choices[0].finish_reason == "stop"`, `object == "chat.completion"`,
   `usage == {"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}`.
7. `gemini_response_to_openai_json_maps_function_call_to_tool_calls` — a
   candidate whose `parts` is `[{"functionCall":{"name":"get_weather","args":{"city":"nyc"}}}]`
   and `finishReason:"STOP"` produces
   `choices[0].message.tool_calls[0] == {"id":<some "call_..." string>,"type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"nyc\"}"}}`
   (note: `arguments` is the *serialized* JSON string, matching OpenAI's
   contract — the inverse of test 3's parse) and
   `choices[0].finish_reason == "tool_calls"` (even though Gemini's own
   `finishReason` said `"STOP"` — assert this override happens whenever any
   part is a `functionCall`, per the design spec's explicit callout that
   this is a presence check, not a string match).
8. `gemini_response_to_openai_json_maps_finish_reasons` —
   `"MAX_TOKENS"`→`"length"`; `"SAFETY"`, `"RECITATION"`,
   `"PROHIBITED_CONTENT"`→`"content_filter"`; anything unrecognized→`"stop"`.
9. `gemini_response_to_openai_json_maps_cached_tokens` — a response whose
   `usageMetadata` includes `"cachedContentTokenCount":4` produces
   `usage.prompt_tokens_details.cached_tokens == 4`.
10. `gemini_chunk_to_openai_chunk_maps_incremental_text` — feed two
    sequential chunks each with a `text` part (`"Hel"` then `"lo"`) through
    the *same* `GeminiStreamState`; assert each call returns a
    `chat.completion.chunk` whose `choices[0].delta.content` equals that
    chunk's own text verbatim (proving no cumulative-diffing is happening —
    Gemini's stream is already delta-shaped per the design spec) and that
    both chunks share the same `id` (state is stable across calls).
11. `gemini_chunk_to_openai_chunk_emits_finish_and_usage_on_the_last_chunk`
    — a chunk carrying `finishReason` + `usageMetadata` (Gemini's own final
    chunk shape) produces a `chat.completion.chunk` with
    `choices[0].finish_reason` set and a top-level `usage` object; an
    intermediate chunk with neither has both absent/`null`.
12. `convert_gemini_sse_to_openai_sse_emits_framed_sse_terminated_by_done`
    — **the critical one.** Feed a `futures::stream::iter` of already-framed
    `data: {...}\n\n` blocks (2-3 canned Gemini chunks), collect the output,
    assert every item starts with `b"data: "` and ends with `b"\n\n"`, each
    payload parses as JSON with `"object":"chat.completion.chunk"`, and the
    final item is exactly `data: [DONE]\n\n` (appended once the upstream
    stream ends — Gemini has no `[DONE]` of its own, mirroring
    `convert_claude_sse_to_openai_sse`'s handling of `message_stop`).
13. `gemini_embedded_error_detects_the_error_object` —
    `{"error":{"message":"boom","code":429}}` → `Some("boom")`; a normal
    candidate response → `None`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib providers::adapter::gemini`
Expected: FAIL to compile (unimplemented stubs / missing module).

- [ ] **Step 3: Implement `transform.rs`**

Mirror `commandcode/transform.rs`'s stream-buffering idea for
`convert_gemini_sse_to_openai_sse` (`futures::stream::unfold`, but simpler
than Command Code's since the input here is already `\n\n`-framed by the
caller — this function only strips the `data: ` prefix and parses JSON per
block, it does not need to buffer partial bytes itself). Mirror
`codex/transform.rs`'s `SseChunkState` for `GeminiStreamState` (stable
`id`/`created` generated once at stream start via
`format!("chatcmpl-{}", uuid::Uuid::new_v4())` and
`chrono::Utc::now().timestamp()` — both already dependencies).

Function-call id synthesis (test 7): use a per-response monotonic counter
inside `gemini_response_to_openai_json`/`GeminiStreamState`
(`format!("call_{n}")`, `n` starting at 0) — Gemini assigns no call id of
its own, so any stable scheme is fine as long as it's unique within one
response.

Tool-result name recovery (test 3): `openai_request_to_gemini` needs to
walk `messages` in order, remembering the most recent
`tool_call_id -> function.name` mapping from any assistant `tool_calls`
entry seen so far, so that a later `{"role":"tool","tool_call_id":...}`
message can look its name back up. An unmatched tool result (no prior
`tool_call_id` seen) is dropped with the `name` field omitted from
`functionResponse` rather than panicking — document this as a known lossy
edge in the function's doc comment, same spirit as Command Code's
"unpaired tool calls are dropped."

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::adapter::gemini::transform`
Expected: PASS, all 13 tests.

- [ ] **Step 5: Commit**

```bash
git add src/providers/adapter/gemini src/providers/adapter/mod.rs
git commit -m "feat(gemini): OpenAI<->Gemini generateContent transform

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: `GeminiAdapter` and adapter registration

Wire Task 2's pure functions into the `ProviderAdapter` trait, including
the Anthropic bridge in both directions (reusing `codex::claude_bridge`
exactly as Command Code's adapter does), and register the kind in
`adapter_for_wire`.

**Files:**
- Create: `src/providers/adapter/gemini/adapter.rs`
- Modify: `src/providers/adapter/gemini/mod.rs` (`pub mod adapter;` +
  `pub use adapter::GeminiAdapter;`, re-export `GEMINI_API_ROOT` for Task 4)
- Modify: `src/providers/adapter/mod.rs` (`adapter_for_wire` match)
- Test: `cargo test --offline --lib providers::adapter::gemini::adapter`

**Interfaces:**
- Consumes: Task 1's `ProviderKind::Gemini`, Task 2's transform functions,
  `codex::claude_bridge::{claude_to_openai_request, openai_to_claude_request,
  convert_openai_sse_to_claude_sse, convert_claude_sse_to_openai_sse,
  openai_json_to_claude_message, claude_json_to_openai_message,
  reframe_sse_blocks}`.
- Produces: `GeminiAdapter::new(provider, http, client_wire)`; module
  const `GEMINI_API_ROOT`. Task 4 uses `GEMINI_API_ROOT`.

- [ ] **Step 1: Write the failing tests**

Copy the harness style from `commandcode/adapter.rs`'s `mod tests` (a
`prov()` helper returning a `Provider` with `kind: Gemini`, `api_key:
Some("g-key-123")`, `base_url: None`, `upstream_model: "gemini-2.0-flash"`;
a `creds()` helper with `api_key: Some("g-key-123")`, everything else
`None`/default). Tests:

1. `build_request_targets_generate_content_with_the_model_in_the_url` — a
   non-streaming client body (`"stream":false` or absent) produces
   `req.url()` ==
   `https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent`,
   method POST, header `x-goog-api-key: g-key-123`, and **no**
   `authorization` or `x-api-key` header.
2. `build_request_targets_stream_generate_content_when_client_asked_to_stream`
   — a body with `"stream":true` produces `req.url()` ending in
   `:streamGenerateContent?alt=sse`, same model/host.
3. `build_request_ignores_client_model_and_uses_upstream_model` — the
   client body's own `"model":"pool-x"` never appears in the request; the
   URL always carries `provider.upstream_model`.
4. `build_request_bridges_an_anthropic_client_body` — with
   `client_wire: Anthropic`, a Claude-shaped
   `{"model":…,"system":"be terse","messages":[…],"max_tokens":64}` body
   produces the same Gemini `contents`/`systemInstruction` shape as the
   equivalent OpenAI body would (i.e. `claude_to_openai_request` really
   runs first, then `openai_request_to_gemini`).
5. `build_request_errors_without_an_api_key` — `creds.api_key: None` →
   `Err(AppError::Internal(_))`.
6. `transform_response_streaming_openai_wire_emits_framed_sse` — build a
   `reqwest::Response` from a canned already-`\n\n`-framed
   `data: {...}\n\n` Gemini SSE body (via `http::Response` →
   `reqwest::Response::from`), `client_wanted_stream = true`, collect the
   body, assert framing + trailing `data: [DONE]\n\n`.
7. `transform_response_streaming_anthropic_wire_emits_claude_events` — same
   input, `client_wire: Anthropic`, assert the output contains
   `event: message_start` … `event: message_stop`. This is the round-trip
   proving `reframe_sse_blocks` → `convert_gemini_sse_to_openai_sse` →
   `convert_openai_sse_to_claude_sse` composes correctly.
8. `transform_response_aggregates_when_client_did_not_stream` —
   `client_wanted_stream = false` → a single JSON body with
   `object == "chat.completion"`; and with a top-level `error` object in
   the response body → `Err(AppError::Upstream(_))` (or the project's
   equivalent variant — check `AppError`'s definition and match its
   existing naming rather than inventing a new one).
9. `needs_refresh_is_always_false` and
   `refresh_credentials_is_an_error` (mirrors `HttpAdapter`'s "passthrough
   has no refresh" — Gemini has no refresh concept either, unlike Command
   Code which echoes credentials back because its access token never
   expires; Gemini's API key similarly never expires from 1router's point
   of view, but there is nothing *to* refresh, so return the same
   `RefreshError::Transient` `HttpAdapter` returns rather than inventing a
   third convention).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib providers::adapter::gemini::adapter` — FAIL to compile.

- [ ] **Step 3: Implement the adapter**

Structure mirrors `commandcode/adapter.rs` one-to-one, minus the OAuth
bits:

- consts:
  ```rust
  pub const GEMINI_API_ROOT: &str = "https://generativelanguage.googleapis.com";
  ```
- `build_request`: parse body → if `client_wire == Anthropic`,
  `claude_to_openai_request` first. Peek the (possibly-bridged) body's
  `"stream"` field (default `false` if absent/non-boolean) to choose
  `:generateContent` vs `:streamGenerateContent?alt=sse`. Run
  `transform::openai_request_to_gemini`. Build
  `format!("{GEMINI_API_ROOT}/v1beta/models/{}:{}", provider.upstream_model, verb)`.
  `.header("x-goog-api-key", api_key)`, no other auth header.
- `transform_response`: if `client_wanted_stream`,
  `claude_bridge::reframe_sse_blocks(upstream.bytes_stream())` →
  `transform::convert_gemini_sse_to_openai_sse(framed, provider.upstream_model.clone())`,
  then, if `client_wire == Anthropic`, wrap in
  `claude_bridge::convert_openai_sse_to_claude_sse`. Otherwise `.text()`/
  parse JSON, check `transform::gemini_embedded_error` first (return
  `AppError::Upstream` if present), else
  `transform::gemini_response_to_openai_json`, and if Anthropic run
  `claude_bridge::openai_json_to_claude_message` on the result.
- `classify_error`: `backoff::classify(status, headers)` — same as every
  other adapter, no bespoke retry logic.
- `needs_refresh`: `false`. `refresh_credentials`: `Err(RefreshError::Transient("gemini has no refresh".into()))`.

Then add the `adapter_for_wire` arm:

```rust
ProviderKind::Gemini => Box::new(
    gemini::GeminiAdapter::new(provider.clone(), http, client_wire),
),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::adapter` — PASS (all existing
adapter tests still green; nothing in `codex/`, `commandcode/`, or `http.rs`
should have changed).

- [ ] **Step 5: Commit**

```bash
git add src/providers/adapter
git commit -m "feat(gemini): GeminiAdapter + adapter_for_wire registration

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: Model discovery

`discover_and_cache_models` and `spawn_bounded_discovery` need a `Gemini`
arm alongside `Passthrough`/`OauthCommandCode`'s, hitting the fixed
`{GEMINI_API_ROOT}/v1beta/models?key=...` endpoint and filtering to
`generateContent`-capable models.

**Files:**
- Modify: `src/providers/routes.rs` (`discover_and_cache_models`,
  `spawn_bounded_discovery`, a new `fetch_gemini_models` sibling to
  `fetch_commandcode_models`, `mod tests` at the bottom)
- Test: `cargo test --offline --lib providers::routes`, plus
  `cargo test --offline --test provider_list_models`

**Interfaces:**
- Consumes: Task 3's `gemini::GEMINI_API_ROOT`, Task 1's kind.
- Produces: `GET /admin/providers/:id/list-models` returning
  `{"ok":true,"models":[…]}` for a Gemini provider; the same list warmed
  into `state.discovered_models` on creation and at boot.

- [ ] **Step 1: Write the failing tests**

- Unit, in `providers::routes::tests`: `parse_gemini_models_body_filters_to_generate_content_capable_models`
  — over
  `{"models":[{"name":"models/gemini-2.0-flash","supportedGenerationMethods":["generateContent"]},{"name":"models/text-embedding-004","supportedGenerationMethods":["embedContent"]}]}`,
  assert the result is `["gemini-2.0-flash"]` (the `"models/"` prefix
  stripped, the embedding-only entry excluded).
- Integration, in `tests/provider_list_models.rs`: a wiremock server
  standing in for the models URL, asserting `list-models` returns
  `ok: true` with the filtered list. Gate the URL behind an overridable
  env var, mirroring Task 4 of the Command Code plan — read
  `ROUTER_GEMINI_API_ROOT` falling back to `GEMINI_API_ROOT`. Env-var tests
  need the `static Mutex` guard pattern used in `core::config`'s tests.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --test provider_list_models` — FAIL (`Gemini`
arm missing, non-exhaustive match compile error until Step 3).

- [ ] **Step 3: Implement the branch**

```rust
let models = match provider.kind {
    ProviderKind::Passthrough => fetch_live_models(&state.http, provider).await?,
    ProviderKind::OauthCommandCode => fetch_commandcode_models(&state.http).await?,
    ProviderKind::Gemini => fetch_gemini_models(&state.http, provider).await?,
    ProviderKind::OauthCodex => {
        return Err("this provider kind has no discoverable /models endpoint".into())
    }
};
```

`fetch_gemini_models` is a sibling of `fetch_commandcode_models`: build
`format!("{root}/v1beta/models?key={}", provider.api_key.as_deref().unwrap_or_default())`
(read `root` from `ROUTER_GEMINI_API_ROOT` or the const), parse with a new
`parse_gemini_models_body` (distinct from the existing OpenAI-shaped
`parse_models_body` — Gemini's list shape is `{"models":[{"name",...}]}`,
not `{"data":[{"id",...}]}`) that strips the `"models/"` prefix and filters
on `supportedGenerationMethods` containing `"generateContent"`. Apply the
identical widening to `spawn_bounded_discovery`'s early-return match.

Update the doc comment on `discover_and_cache_models` (it currently
enumerates why Codex/Command Code are unauthenticated-vs-blocked; add
Gemini's one-line reason: authenticated via `?key=` query param on this one
endpoint specifically, unlike its generate/stream calls which use the
header).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::routes` and
`cargo test --offline --test provider_list_models --test provider_auto_discovery`
— PASS. Confirm the Codex early-return and Command Code/Passthrough paths
are unchanged.

- [ ] **Step 5: Commit**

```bash
git add src/providers/routes.rs tests/provider_list_models.rs
git commit -m "feat(gemini): model discovery branch

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: Onboarding wizard — "Gemini" provider kind

Add a fourth choice to the wizard's provider-kind prompt and the
`add_gemini_provider` step behind it. Unlike Command Code, this is a
simple prompt-for-name-then-key flow — no browser, no listener, no
`spawn_blocking` dance, no CSRF state token.

**Files:**
- Modify: `src/onboarding.rs` (the `Select` at the provider-kind prompt, the
  `match kind` arms, a new `add_gemini_provider` next to
  `add_codex_provider`/`add_commandcode_provider`, `mod tests`)
- Test: `cargo test --offline --lib onboarding`; manual verification per
  `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
- Modify: `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
  (add a Gemini path to the checklist)

**Interfaces:**
- Consumes: Task 1's kind, Task 4's discovery.
- Produces: `pub async fn add_gemini_provider(db, http) -> anyhow::Result<Provider>`.

- [ ] **Step 1: Write the failing test**

`onboarding.rs`'s prompt paths aren't `cargo test`-covered, so this task's
only unit-testable surface is whatever pure helper `add_gemini_provider`
delegates to for persistence — if it's a plain
`queries::insert_provider(&Provider{kind: Gemini, api_key: Some(key), base_url: None, ..})`
call with no bespoke storage function (unlike Command Code's
`store_commandcode_key`, since there's no separate credentials table
involved here), there may be nothing new to unit test beyond what Task 1
already covers. Confirm this rather than inventing a test for a helper that
doesn't need to exist — check whether an existing test like
`add_passthrough_provider_inserts_expected_row` has a Gemini-shaped
counterpart worth adding for symmetry, and if so, write it
(`add_gemini_provider_style_row_has_no_base_url_and_a_flat_api_key` — a
provider constructed the way this step will construct it has
`base_url: None`, `api_key: Some(_)`, `kind: Gemini`).

- [ ] **Step 2: Run to verify it fails (or confirm it's a no-op)**

Run: `cargo test --offline --lib onboarding` — either FAIL to compile (if a
test was added referencing not-yet-written code) or already PASS (if Step 1
concluded no new test was warranted, in which case say so explicitly rather
than skipping the step silently).

- [ ] **Step 3: Implement**

`add_gemini_provider` shape: prompt for a name (used as the id, same
validation `add_passthrough_provider` already applies), prompt for the API
key (`dialoguer::Password`, hidden input — no default/template the way
Passthrough's `PROVIDER_TEMPLATES` offers, since there is exactly one
Gemini endpoint), insert the `Provider` with `kind: Gemini`,
`wire_format: WireFormat::OpenAi` (written but never read by the adapter —
see design spec; default it for cosmetic pool-listing consistency, same as
Passthrough's default), `base_url: None`, `api_key: Some(key)`,
`upstream_model: PENDING_MODEL` initially, then call Task 4's discovery
path (the same discovery helper `add_commandcode_provider` calls) and, if
it returns models, offer them via a `Select`; fall back to a free-text
`Input` (e.g. pre-filled `gemini-2.0-flash`) if discovery fails (bad key,
network down). Persist the chosen model as `upstream_model` via
`queries::update_provider` or an equivalent already-existing helper — do
not hand-roll a second insert path.

Extend the kind `Select` to four items and the `match` to four arms
(check the current wizard's item list/match arms in `onboarding.rs` before
editing — Command Code's own addition already changed a `_ =>` catch-all
into an explicit index, so confirm the current shape rather than assuming
the exact line numbers from the Command Code plan, which predate this
change):

```rust
.items(["passthrough (OpenAI/Anthropic-compatible API key)",
        "Codex OAuth (ChatGPT account)",
        "Command Code (commandcode.ai browser login)",
        "Gemini (Google AI Studio API key)"])
…
0 => add_passthrough_provider(db).await?,
1 => add_codex_provider(db, http).await?,
2 => add_commandcode_provider(db, http).await?,
_ => add_gemini_provider(db, http).await?,
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding` — PASS. Then run the wizard
for real: `cargo run --offline -- setup` against a temp DB, pick "Gemini",
paste a real (or intentionally-wrong, to confirm the failure path) API key,
and confirm model discovery either lists real models or falls back to the
free-text prompt cleanly.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md
git commit -m "feat(onboarding): Gemini provider wizard step

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 6: Admin UI — kind dropdown, conditional fields, discovery filter

The admin UI gets the kind in its dropdown, admission to the discovery
filter, and reuses the plain API-key field the create/edit form already
renders for `passthrough` — no new endpoint, no new panel component
(contrast with Command Code's `CommandCodeKeyPanel`, needed only because
that kind has no plain `api_key` field to reuse).

**Files:**
- Modify: `frontend/src/pages/Providers.tsx` (`KIND_LABELS`, the kind
  `<select>`, the conditional rendering that currently shows `base_url`/
  wire-format fields only for `passthrough` and hides them for the OAuth
  kinds — `gemini` needs the API-key field shown, same as passthrough, but
  the `base_url` field hidden, same as the OAuth kinds, since Gemini's
  endpoint is fixed)
- Modify: `frontend/src/pages/Settings.tsx` (`discoverableProviders`
  filter; the empty-state copy)
- Test: `npm --prefix frontend test` (or the repo's existing frontend test
  command) for the React changes; `cargo test --offline --test admin_providers`
  for `create_provider_accepts_the_new_kind`-style backend coverage

**Interfaces:**
- Consumes: Task 1's kind, Task 4's discovery.
- Produces: no new backend route — `POST /admin/providers` already accepts
  an arbitrary `kind` string via `CreateBody`; a Gemini row is just
  `{"kind":"gemini","api_key":"...","base_url":null,...}` through the
  existing endpoint.

- [ ] **Step 1: Write the failing tests**

Integration, in `tests/admin_providers.rs` (uses `tests/common::spawn_app`,
sandbox-blocked, see Global Constraints):
1. `create_provider_accepts_gemini_kind` — `POST /admin/providers` with
   `"kind":"gemini","api_key":"g-key","base_url":null,...` → 201, and the
   round-tripped `kind` in the response is `"gemini"`, `credential_configured: true`.
2. `list_providers_masks_the_gemini_api_key` — confirm the existing `mask`
   helper's masking behavior (already generic over any non-OAuth kind)
   covers `gemini` with no code change — write the test to *prove* this
   rather than assuming it, since `is_oauth_kind` not including `Gemini` is
   exactly the thing this test guards against regressing.

Frontend: extend `Providers.test.tsx`/`Settings.test.tsx` (or whatever the
existing test files are actually named — check first) to assert the
dropdown contains a `gemini` option, that selecting it hides the `base_url`
field, and that a Gemini provider appears in the Settings discovery list.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --test admin_providers` and the frontend test
command — the backend test should already pass (no backend code changes
needed per this task's Interfaces note; if it doesn't, something in Task 1
or the `CreateBody` deserialization is incomplete — investigate before
assuming Step 3 needs a backend change). The frontend tests fail until
Step 3.

- [ ] **Step 3: Implement the frontend changes**

- `KIND_LABELS`: add `gemini: "Gemini (API key)"`.
- The kind `<select>`: add `<option value="gemini">{KIND_LABELS.gemini}</option>`.
- Wherever the form conditionally shows `base_url`/wire-format-of-upstream
  fields for `passthrough` and hides them for OAuth kinds, add `gemini` to
  the "hide `base_url`" set (its endpoint is fixed) while keeping it in the
  "show `api_key`" set (unlike the OAuth kinds, which show a panel/no field
  at all).
- `Settings.tsx`: change the discovery filter to
  `providers.filter((p) => p.kind === "passthrough" || p.kind === "oauth_command_code" || p.kind === "gemini")`
  and update the empty-state copy if it enumerates kinds by name.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --test admin_providers` (outside the sandbox)
and the frontend test command. Then build the UI and click through:
create a Gemini provider, paste a key, save, and confirm the Settings
page's "Check providers for available models" lists its models.

- [ ] **Step 5: Commit**

```bash
git add frontend/src tests/admin_providers.rs
git commit -m "feat(admin): Gemini kind in provider dropdown + discovery filter

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 7: End-to-end integration test and documentation

Prove a real proxy request survives the whole path, and record the new
provider in the docs the next contributor reads.

**Files:**
- Create: `tests/gemini_proxy.rs`
- Modify: `README.md` (provider list / setup section)
- Modify: `CLAUDE.md` (doc index — add this plan and its design spec;
  correct any remaining "the adapters are Codex/CommandCode" enumeration)
- Test: `cargo test --offline --test gemini_proxy`

**Interfaces:**
- Consumes: everything above.
- Produces: no new code surface — the only production change in this task
  is threading `ROUTER_GEMINI_API_ROOT` through `gemini::adapter` too
  (Task 4 already added it for discovery; the generate/stream calls need
  the same override so `wiremock` can stand in for
  `generativelanguage.googleapis.com` in tests), if that wasn't already
  done in Task 3/4.

> **⚠ Sandbox note:** `tests/common::spawn_app` binds a real socket, so
> this whole file is sandbox-blocked. Re-run it outside a Codex sandbox.

- [ ] **Step 1: Write the failing tests**

In `tests/gemini_proxy.rs`, using `wiremock` as the fake Gemini upstream:

1. `openai_wire_streaming_end_to_end` — `POST /v1/chat/completions` with
   `stream:true` against a pool backed by a Gemini provider; upstream
   replies with canned already-`\n\n`-framed `data: {...}` SSE chunks (2-3
   text chunks + a final chunk with `finishReason`/`usageMetadata`); assert
   the client sees framed `data: {"object":"chat.completion.chunk",...}`
   chunks ending in `data: [DONE]`, and that the wiremock request hit
   `.../streamGenerateContent` with `x-goog-api-key` set and **no**
   `authorization` header.
2. `anthropic_wire_streaming_end_to_end` — `POST /v1/messages` against an
   `anthropic`-wire pool; assert `event: message_start` … `event: message_stop`.
3. `non_streaming_aggregates` — `stream:false` (or absent) → a single
   `chat.completion` JSON body, and the wiremock request hit
   `.../generateContent` (not the stream variant).
4. `function_call_round_trips_through_the_openai_wire` — a client sends
   `tools`/`tool_choice`, wiremock's canned response includes a
   `functionCall` part, assert the client sees
   `choices[0].message.tool_calls` with a JSON-string `arguments` field and
   `finish_reason == "tool_calls"`.
5. `upstream_429_cools_the_provider_and_fails_over` — a second passthrough
   provider at lower priority serves the request, proving `classify_error`
   delegates to `proxy::backoff` and the adapter doesn't swallow retries.
6. `a_400_with_no_api_key_is_reported_as_misconfigured` — assert via
   `GET /admin/providers/:id/state` (or whatever the project's equivalent
   status surface is — check `admin_providers`/`health_stats` tests for the
   exact endpoint name before writing this) that a Gemini provider with no
   `api_key` set is reported as misconfigured rather than crashing the
   request path.

- [ ] **Step 2: Run to verify it fails**

Run (outside the sandbox): `cargo test --offline --test gemini_proxy` — FAIL.

- [ ] **Step 3: Make the API root overridable (if not already) and fix the docs**

- `README.md`: list Gemini among the supported provider kinds (currently:
  "OpenAI, Anthropic, DeepSeek, OpenCode, a ChatGPT account, or a Command
  Code account" in the intro paragraph, and the wizard walkthrough's
  provider-kind bullet list) — add "a Gemini API key" to both, and add a
  one-line note next to the existing "Wire format" callout that Gemini,
  like Codex/Command Code, translates rather than requiring a matching
  `wire_format`.
- `CLAUDE.md`: this plan's design spec + implementation plan join the
  bulleted doc index at the top of the file, in the same "design: ...,
  plan: ..." format as the other entries.

- [ ] **Step 4: Run to verify it passes**

Run (outside the sandbox): `cargo test --offline` — the full suite, PASS.
Confirm no pre-existing test regressed, in particular `codex_oauth`,
`commandcode_proxy`, `proxy_failover`, `proxy_streaming`,
`admin_export_import` (round-trips `kind` through JSON), and
`provider_auto_discovery`.

- [ ] **Step 5: Commit**

```bash
git add tests/gemini_proxy.rs src/providers/adapter/gemini README.md CLAUDE.md
git commit -m "test(gemini): end-to-end proxy coverage; docs

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage** — every section of
`docs/superpowers/specs/2026-08-05-gemini-provider-design.md` maps to a task:

| Spec section | Task |
|---|---|
| Why a bespoke adapter | Tasks 2, 3 |
| Reuse: `claude_bridge` in both directions, `reframe_sse_blocks` | Task 3 |
| Reuse: no inner retry loop, `proxy::backoff` owns it | Task 3 (`classify_error`), Task 7 test 5 |
| Reuse: no OAuth machinery, plain `api_key` column, no migration | Task 1, Task 5 |
| Request translation table (roles, tools, tool_choice, generationConfig) | Task 2 tests 1-5 |
| Response translation table (text, function calls, finish reasons, usage) | Task 2 tests 6-9 |
| Streaming translation (delta-shaped, stable id, `[DONE]` synthesis) | Task 2 tests 10-12 |
| Endpoint/auth/model-discovery conventions | Task 3 (build_request), Task 4 |
| Admin UI scope (no new endpoint, reuse `api_key` field) | Task 6 |
| Explicitly out of scope | Global Constraints |

**2. Placeholder scan** — no `TBD`, no "add appropriate ...". Every URL,
header name, JSON key, and enum spelling is given literally, taken from the
design spec's tables.

**3. Name consistency across tasks** — `ProviderKind::Gemini` / DB+JSON
value `gemini` appears identically in Tasks 1, 3, 4, 5, 6. `GEMINI_API_ROOT`
is defined in Task 3 and consumed in Task 4/7. The env override
`ROUTER_GEMINI_API_ROOT` is introduced once (Task 4) and reused (Task 7).

**4. Sequencing** — Task 1 must land first (every other task matches on the
variant). Task 2 is leaf-parallel with nothing (pure functions, no
dependency on Task 1). Task 3 joins 1+2. Task 4 needs 3 (for
`GEMINI_API_ROOT`). Task 5 needs 4 (discovery) and 1. Task 6 needs 1 (kind
exists) but not 4/5 strictly — could run in parallel with them. Task 7
joins everything.

**5. Deliberate asymmetry vs. the Command Code plan this mirrors** — no
Task equivalent to Command Code's Task 5 (`browser_login.rs`) exists at
all, and no Task equivalent to Command Code's Task 7's new admin endpoint
exists either, because Gemini's credential model (flat API key in an
existing column) needs neither. This is the intended simplification the
design spec calls out, not an oversight — do not add either back in.

### Critical Files for Implementation
- /home/ducph/duc/1router/src/providers/adapter/commandcode/adapter.rs
- /home/ducph/duc/1router/src/providers/adapter/commandcode/transform.rs
- /home/ducph/duc/1router/src/providers/adapter/codex/claude_bridge.rs
- /home/ducph/duc/1router/src/providers/adapter/mod.rs
- /home/ducph/duc/1router/src/core/model.rs
- /home/ducph/duc/1router/src/providers/routes.rs
- /home/ducph/duc/1router/src/onboarding.rs
- /home/ducph/duc/1router/docs/superpowers/specs/2026-08-05-gemini-provider-design.md
