# Command Code Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add commandcode.ai as a first-class preset provider — a new `ProviderKind::OauthCommandCode` served by a bespoke adapter that translates between the OpenAI Chat Completions shape 1router speaks internally and Command Code's proprietary `/alpha/generate` + NDJSON protocol, with a browser login in `1router setup` and a paste-the-key path in the admin UI.

**Architecture:** One new adapter module, `src/providers/adapter/commandcode/` (`adapter.rs`, `transform.rs`, `browser_login.rs`, `mod.rs`), mirroring `src/providers/adapter/codex/`'s breakdown minus `oauth.rs`/`refresh.rs` (there is no token exchange and no refresh). Everything else is gate-widening: one new `ProviderKind` variant, one new `adapter_for_wire` arm, one discovery branch, one wizard branch, one admin endpoint, and the frontend dropdown/filters. Credentials reuse `provider_oauth_state` unchanged — **no migration**. The Anthropic wire is served by reusing `codex::claude_bridge` verbatim in both directions.

**Tech Stack:** Existing deps only — `reqwest`, `serde_json`, `futures`, `bytes`, `tokio` (incl. `tokio::net::TcpListener`, `tokio::time::timeout`, `tokio::task::spawn_blocking`), `uuid`, `urlencoding`, `dialoguer`, `chrono`. **No new crate is added** — the browser is opened with `std::process::Command`, specifically so that no online `cargo fetch` is needed before `--offline` work.

## Global Constraints

- Package is `router`, binary is `1router`; import via `use router::...`. Build/test with `cargo build --offline` / `cargo test --offline`.
- **No new Cargo dependency.** If you find yourself reaching for an `open`/`webbrowser` crate, stop: Task 5 specifies `std::process::Command`. Adding any dep requires a real-network `cargo fetch` (including transitive dev-deps) before any `--offline` build can succeed.
- **No migration.** `providers.kind` is `TEXT NOT NULL DEFAULT 'passthrough'` with no `CHECK` constraint (`migrations/0001_init.sql:5`), so a new `ProviderKind` variant needs no schema change. Credentials go in the existing `provider_oauth_state` table.
- DB value of the new kind is exactly `oauth_command_code` (snake_case via the existing `#[sqlx(rename_all = "snake_case")]` / `#[serde(rename_all = "snake_case")]` on `ProviderKind`).
- Fixed upstream constants, used verbatim:
  - generate: `https://api.commandcode.ai/alpha/generate`
  - models: `https://api.commandcode.ai/provider/v1/models` (unauthenticated)
  - studio auth: `https://commandcode.ai/studio/auth/cli?callback=<urlencoded>&state=<token>`
- Auth is `Authorization: Bearer {creds.access_token}` — **not** `creds.api_key`, which stays `NULL` for this kind.
- All emitted SSE must be framed as `data: {json}\n\n` and terminated with `data: [DONE]\n\n`. Raw NDJSON handed to `claude_bridge::convert_openai_sse_to_claude_sse` silently yields an empty Anthropic stream.
- Do **not** port the reference implementation's inner 429/5xx retry loop — `proxy::backoff` + `proxy::flow`'s failover own that.
- `refresh_task.rs`'s loop and `proxy/flow.rs`'s `AuthExpired` recovery branch stay `ProviderKind::OauthCodex`-only. Do not add the new kind to either — that is intentional, not an omission.
- axum is pinned to **0.7**: any new route uses `:id`, never `{id}`. Getting this wrong 404s silently instead of failing to compile.
- Tests that bind a real `TcpListener` (Task 5's browser-login tests, and any test using `tests/common::spawn_app`) are **BLOCKED in a Codex sandbox** (`TcpListener::bind` → `PermissionDenied`). Re-run those specific tests outside the sandbox before calling the task done.
- With the `ui` feature default-on, any `cargo build/test --offline` dispatched into a Codex worktree must add `--no-default-features` (no `node`/`npm` there for `build.rs`).
- Reference implementation (read-only, for wire-protocol details): `/tmp/claude-1000/-home-ducph-duc-1router/b1ae10c6-9f42-440b-bbc7-5076dde406c9/scratchpad/pi-commandcode-provider` — `src/core.ts`, `src/converters.ts`, `src/oauth.ts`, `src/auth-server.ts`, `src/models.ts`.
- Out of scope (v1): cost accounting beyond the `usage` object, key rotation/refresh, browser login from the admin UI, honouring upstream `tool-result` events.

---

### Task 1: `ProviderKind::OauthCommandCode` and the two gates that must widen

Introduce the variant and widen exactly the two kind-gates that would
otherwise strand it: `Provider::supports_wire` (both `pools/routes.rs`
and `pools/select.rs` gate on it, so a `false` here makes the provider
unroutable) and `queries::update_provider`'s wire_format-flip guard.
Deliberately leaves `refresh_task.rs` and `proxy/flow.rs` alone.

**Files:**
- Modify: `src/core/model.rs` (`ProviderKind` enum ~line 16; `supports_wire` ~line 34; tests at the bottom)
- Modify: `src/providers/queries.rs` (`update_provider`, the `p.kind != ProviderKind::OauthCodex` condition ~line 79)
- Test: `cargo test --offline --lib core::model` and `--lib providers::queries`

**Interfaces:**
- Consumes: nothing.
- Produces: `ProviderKind::OauthCommandCode` (serializes to `"oauth_command_code"`), a `supports_wire` that returns `true` for it on both wire formats. Tasks 3, 4, 6, 7 all match on this variant.

- [ ] **Step 1: Write the failing tests**

In `src/core/model.rs`'s `mod tests`, add:
- `command_code_kind_serializes_as_oauth_command_code` — `serde_json::to_string(&ProviderKind::OauthCommandCode)` == `"\"oauth_command_code\""`, and the round-trip back.
- extend `provider_supports_wire_depends_on_kind` (or add a sibling) asserting a provider with `kind: OauthCommandCode` returns `true` for both `WireFormat::OpenAi` and `WireFormat::Anthropic`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib core::model`
Expected: FAIL to compile — `no variant named 'OauthCommandCode' found for enum 'ProviderKind'`.

- [ ] **Step 3: Add the variant and widen `supports_wire`**

Add `OauthCommandCode` to `ProviderKind`. Change `supports_wire`'s match to put both OAuth kinds in the `true` arm:

```rust
match self.kind {
    ProviderKind::OauthCodex | ProviderKind::OauthCommandCode => true,
    ProviderKind::Passthrough => self.wire_format == w,
}
```

Fix any resulting non-exhaustive-match compile errors *only* where the compiler demands it (`providers/adapter/mod.rs` is handled in Task 3 — a `todo!()`/`unimplemented!()` placeholder there is acceptable until then, but must be gone by the end of Task 3).

- [ ] **Step 4: Widen the wire_format-flip guard**

In `src/providers/queries.rs::update_provider`, change

```rust
if w != p.wire_format && p.kind != ProviderKind::OauthCodex {
```

to skip the pool-homogeneity check for both OAuth kinds (e.g.
`&& !matches!(p.kind, ProviderKind::OauthCodex | ProviderKind::OauthCommandCode)`).
Rationale to keep in the comment: credentials live in `provider_oauth_state`
keyed by provider id, not wire_format, so flipping wire_format strands nothing.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --offline --lib` — expected PASS, including a new/updated `update_provider` test asserting a wire_format flip on an `oauth_command_code` provider that is a member of an opposite-wire pool succeeds (mirror the existing Codex case if one exists; otherwise write it).

- [ ] **Step 6: Commit**

```bash
git add src/core/model.rs src/providers/queries.rs
git commit -m "feat(providers): add ProviderKind::OauthCommandCode

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: `commandcode::transform` — NDJSON → framed OpenAI SSE

The pure-function half of the adapter, testable with zero network. Two
directions: OpenAI-shaped request JSON → Command Code `params`, and
Command Code NDJSON events → framed `chat.completion.chunk` SSE (plus a
non-streaming aggregate).

**Files:**
- Create: `src/providers/adapter/commandcode/mod.rs`
- Create: `src/providers/adapter/commandcode/transform.rs`
- Modify: `src/providers/adapter/mod.rs` (add `pub mod commandcode;` — the `adapter_for_wire` arm lands in Task 3)
- Test: `cargo test --offline --lib providers::adapter::commandcode::transform`

**Interfaces:**
- Consumes: Task 1's enum only indirectly.
- Produces, for Task 3:
  - `pub fn transform_request(client_json: &Value, thread_id: &str, working_dir: &str) -> Value` — the full Command Code body.
  - `pub fn parse_event_line(line: &str) -> Option<Value>` — tolerant NDJSON/`data:` line parser.
  - `pub struct ChunkState` + `pub fn chat_chunk_for_event(&mut ChunkState, event: &Value, model: &str) -> Option<Value>`.
  - `pub fn convert_ndjson_stream<S, E>(upstream: S, model: String) -> impl Stream<Item = Result<Bytes, E>>`.
  - `pub fn aggregate_ndjson(body: &str, model: &str) -> Value` and `pub fn ndjson_embedded_error(body: &str) -> Option<String>`.
  - `pub fn project_slug_from_path(path: &str) -> String`.

- [ ] **Step 1: Write the failing tests**

Create `transform.rs` with only a `#[cfg(test)] mod tests` and stub signatures. Tests to write (all `#[test]`, no async needed except the stream ones which use `#[tokio::test]` + `futures::stream::iter`):

1. `transform_request_builds_the_command_code_envelope` — given an OpenAI body `{"model":"pool-x","messages":[{"role":"user","content":"hi"}],"max_tokens":100,"stream":true}`, assert the output has top-level keys `config`, `memory`, `taste`, `skills`, `params`, `threadId`; that `params.stream == true` regardless of the client's `stream`; `params.messages[0] == {"role":"user","content":"hi"}`; `params.temperature == 0.3`; `params.max_tokens == 100`; `memory`/`taste`/`skills` are `null`; `threadId` equals the passed value.
2. `transform_request_lifts_system_messages_into_params_system` — a leading `{"role":"system","content":"be terse"}` ends up in `params.system` as a string and is removed from `params.messages`.
3. `transform_request_converts_tools_and_tool_calls` — OpenAI `tools:[{type:"function",function:{name,description,parameters}}]` becomes `[{type:"function",name,description,input_schema}]`; an assistant message with `tool_calls` becomes `content:[{type:"tool-call",toolCallId,toolName,input}]`; a `{"role":"tool","tool_call_id":"t1","content":"ok"}` becomes `{"role":"tool","content":[{type:"tool-result",toolCallId:"t1",output:{type:"text",value:"ok"}}]}`. Unpaired tool calls (a `tool-call` with no matching `tool-result`, or vice versa) are dropped — see `completeToolCallIds` in the reference `converters.ts`.
4. `parse_event_line_is_tolerant` — accepts a raw `{"type":"text-delta","text":"a"}` line, accepts the same line with a `data: ` prefix, returns `None` for `""`, `":comment"`, `"event: foo"`, and `"data: [DONE]"`.
5. `chat_chunk_for_event_maps_text_and_reasoning` — `text-delta` → `delta.content`; `reasoning-delta` → `delta.reasoning_content` (a `reasoning-start`/`reasoning-end` pair emits nothing on its own).
6. `chat_chunk_for_event_maps_tool_calls` — a `tool-call` event → `delta.tool_calls:[{index:0,id,type:"function",function:{name,arguments:"<json string>"}}]`; `arguments` is the *serialized* `input` object, matching OpenAI's contract.
7. `finish_event_maps_usage_and_finish_reason` — `{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":100,"outputTokens":20,"inputTokenDetails":{"noCacheTokens":40,"cacheReadTokens":50,"cacheWriteTokens":10}}}` produces a final chunk with `finish_reason == "tool_calls"` and `usage == {prompt_tokens:100, completion_tokens:20, total_tokens:120, prompt_tokens_details:{cached_tokens:50}}`. Also assert `"length"`/`"max_tokens"`/`"max-tokens"`/`"max_output_tokens"` all map to `"length"` and anything else to `"stop"`.
8. `convert_ndjson_stream_emits_framed_sse_terminated_by_done` — **the critical one.** Feed the stream bytes split at deliberately awkward boundaries (mid-JSON, mid-newline), collect the output, and assert every item starts with `b"data: "` and ends with `b"\n\n"`, that each item's payload parses as JSON with `"object":"chat.completion.chunk"`, and that the final item is exactly `data: [DONE]\n\n`.
9. `aggregate_ndjson_builds_a_chat_completion` — concatenated text deltas land in `choices[0].message.content`, `object == "chat.completion"`, usage carried over.
10. `ndjson_embedded_error_detects_the_error_event` — `{"type":"error","error":{"message":"boom"}}` → `Some("boom")`; also handles `"error":"boom"` as a bare string.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib providers::adapter::commandcode`
Expected: FAIL to compile (unimplemented stubs / missing module).

- [ ] **Step 3: Implement `transform.rs`**

Mirror `codex/transform.rs` closely — same `render_chunk`/`SSE_DONE`
shape (`format!("data: {chunk}\n\n")`, `b"data: [DONE]\n\n"`), same
`futures::stream::unfold` buffering pattern in `convert_ndjson_stream`
(buffer a `String`, split on `\n` instead of Codex's `\n\n`, since
upstream is NDJSON), same `ChunkState` idea (stable `id`/`created`
generated once at stream start; `saw_tool_call` decides the default
`finish_reason`).

The request envelope, verbatim from `core.ts`:

```rust
json!({
    "config": {
        "workingDir": working_dir,
        "date": Utc::now().format("%Y-%m-%d").to_string(),
        "environment": environment_info(),   // e.g. "linux-x86_64, 1router/<version>"
        "structure": [],
        "isGitRepo": false,
        "currentBranch": "",
        "mainBranch": "",
        "gitStatus": "",
        "recentCommits": []
    },
    "memory": Value::Null,
    "taste": Value::Null,
    "skills": Value::Null,
    "params": {
        "model": model,
        "messages": messages,
        "tools": tools,
        "system": system,
        "max_tokens": max_tokens,   // default 64_000, clamped
        "temperature": 0.3,
        "stream": true              // always, regardless of the client
    },
    "threadId": thread_id
})
```

`project_slug_from_path` lowercases, strips a leading Windows drive
letter, replaces every non-`[a-z0-9]` run with `-`, trims leading/trailing
`-`, and falls back to `"project"`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::adapter::commandcode::transform`
Expected: PASS, all 10 tests.

- [ ] **Step 5: Commit**

```bash
git add src/providers/adapter/commandcode src/providers/adapter/mod.rs
git commit -m "feat(commandcode): NDJSON<->OpenAI SSE transform

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: `CommandCodeAdapter` and adapter registration

Wire Task 2's pure functions into the `ProviderAdapter` trait, including
the Anthropic bridge in both directions, and register the kind in
`adapter_for_wire`.

**Files:**
- Create: `src/providers/adapter/commandcode/adapter.rs`
- Modify: `src/providers/adapter/commandcode/mod.rs` (`pub mod adapter;` + `pub use adapter::CommandCodeAdapter;`, and re-export `DEFAULT_MODELS_URL` for Task 4)
- Modify: `src/providers/adapter/mod.rs` (`adapter_for_wire` match, ~line 51)
- Test: `cargo test --offline --lib providers::adapter::commandcode::adapter`

**Interfaces:**
- Consumes: Task 1's `ProviderKind::OauthCommandCode`, Task 2's transform functions, `codex::claude_bridge::{claude_to_openai_request, convert_openai_sse_to_claude_sse, openai_json_to_claude_message}`.
- Produces: `CommandCodeAdapter::new(provider, http, client_wire)`; module consts `GENERATE_URL`, `DEFAULT_MODELS_URL`, `COMMAND_CODE_VERSION`. Task 4 uses `DEFAULT_MODELS_URL`.

- [ ] **Step 1: Write the failing tests**

Copy the harness style from `codex/adapter.rs`'s `mod tests` (a `prov()`
helper returning a `Provider` with `kind: OauthCommandCode`, `api_key:
None`, `base_url: None`; a `creds()` helper with
`access_token: Some("cc-key-123")`). Tests:

1. `build_request_targets_generate_with_fixed_headers` — asserts `req.url()` == `https://api.commandcode.ai/alpha/generate`, method POST, `authorization: Bearer cc-key-123`, and the presence and exact values of `x-command-code-version`, `x-cli-environment: production`, `x-project-slug`, `x-taste-learning: true`, `x-co-flag: false`.
2. `build_request_rewrites_model_to_the_upstream_model` — the client's `model` (a pool id) is replaced by `provider.upstream_model` in `params.model`, matching `CodexAdapter`'s behaviour.
3. `build_request_uses_access_token_not_api_key` — with `api_key: Some("WRONG")` on the provider *and* `access_token: Some("cc-key-123")`, the header carries the access token. With `access_token: None`, `build_request` returns `AppError::Internal`.
4. `build_request_bridges_an_anthropic_client_body` — with `client_wire: Anthropic`, a Claude-shaped `{"model":…,"system":"be terse","messages":[…],"max_tokens":64}` body produces the same `params` shape as the equivalent OpenAI body (i.e. `claude_to_openai_request` really runs first).
5. `transform_response_streaming_openai_wire_emits_framed_sse` — build a `reqwest::Response` from a canned NDJSON body (via `http::Response` → `reqwest::Response::from`), `client_wanted_stream = true`, collect the body, assert framing + `[DONE]`.
6. `transform_response_streaming_anthropic_wire_emits_claude_events` — same input, `client_wire: Anthropic`, assert the output contains `event: message_start`, `event: content_block_delta`, `event: message_stop`. This is the round-trip that proves the framing contract holds.
7. `transform_response_aggregates_when_client_did_not_stream` — `client_wanted_stream = false` → a single JSON body with `object == "chat.completion"`; and with an `error` event in the NDJSON → `Err(AppError::Upstream)`.
8. `needs_refresh_is_always_false` and `refresh_credentials_echoes_the_credentials_back`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib providers::adapter::commandcode::adapter` — FAIL to compile.

- [ ] **Step 3: Implement the adapter**

Structure mirrors `codex/adapter.rs` one-to-one:

- consts:
  ```rust
  const GENERATE_URL: &str = "https://api.commandcode.ai/alpha/generate";
  pub const DEFAULT_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";
  const COMMAND_CODE_VERSION: &str = "0.29.0"; // CLI version Command Code expects
  ```
- `build_request`: parse body → if `client_wire == Anthropic`, `claude_to_openai_request` → `transform::transform_request` → force `params.model = provider.upstream_model` → POST with `.bearer_auth(access)` + the five fixed headers. `threadId` is a fresh `Uuid::new_v4()` per request; `x-project-slug` is `project_slug_from_path` of the process cwd (fall back to `"project"` if cwd is unavailable).
- `transform_response`: `client_wanted_stream` → `transform::convert_ndjson_stream(upstream.bytes_stream().boxed(), model)`, then, if Anthropic, wrap in `claude_bridge::convert_openai_sse_to_claude_sse`. Otherwise read `.text()`, check `ndjson_embedded_error`, `aggregate_ndjson`, and if Anthropic run `openai_json_to_claude_message`.
- `classify_error`: `backoff::classify(status, headers)` — same as Codex, no bespoke retry logic.
- `needs_refresh`: `false`. `refresh_credentials`: `Ok(creds.clone())` with a comment that the background loop and the `AuthExpired` branch both exclude this kind, so it is never invoked.

Then add the `adapter_for_wire` arm:

```rust
ProviderKind::OauthCommandCode => Box::new(
    commandcode::CommandCodeAdapter::new(provider.clone(), http, client_wire),
),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::adapter` — PASS (all Codex tests still green too; nothing in `codex/` should have changed).

- [ ] **Step 5: Commit**

```bash
git add src/providers/adapter
git commit -m "feat(commandcode): CommandCodeAdapter + adapter_for_wire registration

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: Unauthenticated model discovery

`discover_and_cache_models` and `spawn_bounded_discovery` currently
early-return for any non-`Passthrough` kind. Admit the new kind through a
dedicated branch that hits the fixed `DEFAULT_MODELS_URL` with **no** auth
header, rather than trying to teach `derive_models_url` about
`/alpha/generate`.

**Files:**
- Modify: `src/providers/routes.rs` (`fetch_live_models` ~line 225, `discover_and_cache_models` ~line 284, `spawn_bounded_discovery` ~line 302, `mod tests` at the bottom)
- Test: `cargo test --offline --lib providers::routes`, plus `cargo test --offline --test provider_list_models`

**Interfaces:**
- Consumes: Task 3's `commandcode::DEFAULT_MODELS_URL`, Task 1's kind.
- Produces: `GET /admin/providers/:id/list-models` returning `{"ok":true,"models":[…]}` for a Command Code provider; the same list warmed into `state.discovered_models` on creation and at boot, so `GET /v1/models` lists `<provider_id>/<model>`.

- [ ] **Step 1: Write the failing tests**

- Unit, in `providers::routes::tests`: `parse_models_payload_ignores_extra_fields` — the shared parse step over `{"object":"list","data":[{"id":"cc-1","name":"CC One","context_length":200000}]}` yields `["cc-1"]` (extract the parsing out of `fetch_live_models` into a small `fn parse_models_body(body: &Value) -> Result<Vec<String>, String>` so both paths share it and it is unit-testable).
- Integration, in `tests/provider_list_models.rs`: a wiremock server standing in for the models URL, asserting the outgoing request carries **no** `authorization` and no `x-api-key` header, and that `list-models` returns `ok: true`. To make this testable without real network, gate the URL behind an overridable value — read `ROUTER_COMMANDCODE_MODELS_URL` (env) falling back to `DEFAULT_MODELS_URL`. Env-var-touching tests need the `static Mutex` guard pattern used in `core::config`'s tests.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --test provider_list_models` — FAIL: `{"ok":false,"reason":"this provider kind has no discoverable /models endpoint"}`.

- [ ] **Step 3: Implement the branch**

In `discover_and_cache_models`, replace the flat guard with a match on kind:

```rust
let models = match provider.kind {
    ProviderKind::Passthrough => fetch_live_models(&state.http, provider).await?,
    ProviderKind::OauthCommandCode => fetch_commandcode_models(&state.http).await?,
    ProviderKind::OauthCodex => {
        return Err("this provider kind has no discoverable /models endpoint".into())
    }
};
```

`fetch_commandcode_models` is a small sibling of `fetch_live_models`: no
`base_url`, no auth header, same `parse_models_body` on the way out.
Apply the identical widening to `spawn_bounded_discovery`'s early return
so creation-time and boot-time warm-up cover the new kind too.

Update the doc comments on both functions (they currently say
"Codex OAuth has no discoverable models endpoint" as the sole
justification for the gate).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib providers::routes` and
`cargo test --offline --test provider_list_models --test provider_auto_discovery` — PASS. Confirm the Codex early-return is unchanged (`oauth_codex` still reports `ok:false`).

- [ ] **Step 5: Commit**

```bash
git add src/providers/routes.rs tests/provider_list_models.rs
git commit -m "feat(commandcode): unauthenticated model discovery branch

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: `browser_login` — one-shot local callback listener

The CLI-side login. Binds a local listener, hands the user a studio URL,
accepts exactly one CORS-preflighted `POST /callback`, validates the CSRF
state token, and falls back to a paste prompt on timeout.

**Files:**
- Create: `src/providers/adapter/commandcode/browser_login.rs`
- Modify: `src/providers/adapter/commandcode/mod.rs`
- Test: `cargo test --offline --lib providers::adapter::commandcode::browser_login`

> **⚠ Codex sandbox note:** every test in this task binds a real
> `TcpListener` on `127.0.0.1` and will report BLOCKED/FAILED with
> `PermissionDenied` inside a Codex worktree even when the code is
> correct. **Re-run this module's tests yourself outside the sandbox
> before marking the task done** — a green sandbox run is not achievable
> and a red one is not evidence of a bug.

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces, for Tasks 6 and 7:
  - `pub struct AuthCallback { pub api_key: String, pub state: String, pub user_id: String, pub user_name: String, pub key_name: String }`
  - `pub struct AuthListener { pub port: u16, pub state_token: String }` with `pub fn authorize_url(&self) -> String` and `pub async fn wait(self) -> Result<AuthCallback, LoginError>`
  - `pub async fn bind_listener() -> std::io::Result<(TcpListener, u16)>`
  - `pub fn open_in_browser(url: &str)` (best-effort, never fails the flow)
  - `pub const TEN_YEARS_SECS` / `pub fn far_future_expiry() -> DateTime<Utc>`
  - `pub fn sanitize_api_key(input: &str) -> String`

- [ ] **Step 1: Write the failing tests**

All `#[tokio::test]`, each driving the listener with a `reqwest` client
against `http://127.0.0.1:{port}`:

1. `bind_listener_scans_from_5959` — bind once, assert the port is in `5959..=5968` or (if all ten were busy) non-zero; then hold that listener and bind again, asserting the second bind picks a *different* port.
2. `options_preflight_returns_204_with_cors_headers` — send `OPTIONS /callback` with `Origin: https://commandcode.ai`; assert 204, `access-control-allow-origin: https://commandcode.ai`, `access-control-allow-methods: POST, OPTIONS`, `access-control-allow-private-network: true`.
3. `preflight_origin_falls_back_for_an_unknown_origin` — `Origin: https://evil.example` → `access-control-allow-origin: http://localhost:3000` (the allowlist's last/default entry).
4. `post_callback_resolves_with_all_five_fields` — a complete body resolves `wait()` to an `AuthCallback` and returns `{"success":true}` / 200.
5. `post_callback_rejects_an_incomplete_body` — each of the five fields missing or empty in turn → HTTP 400 `{"success":false,…}`, and `wait()` does *not* resolve.
6. `post_callback_rejects_malformed_json` → 400.
7. `wrong_path_and_wrong_method_are_rejected` — `GET /callback` → 405, `POST /nope` → 404.
8. `state_mismatch_is_rejected_by_the_caller` — drive the full helper with a callback whose `state` differs from the issued token; assert `LoginError::StateMismatch`.
9. `wait_times_out_after_the_configured_duration` — with the timeout overridden to ~100ms and nothing posted, `wait()` returns `LoginError::Timeout`.
10. `authorize_url_is_correctly_encoded` — pure, no socket: asserts the URL is `https://commandcode.ai/studio/auth/cli?callback=http%3A%2F%2Flocalhost%3A5959%2Fcallback&state=<token>` (use `urlencoding::encode`, already a dependency).
11. `sanitize_api_key_strips_bracketed_paste_markers` — pure: strips `\x1b[200~`/`\x1b[201~`/bare `[200~`/`[201~`, drops control chars, trims.

- [ ] **Step 2: Run to verify it fails**

Run (outside any Codex sandbox): `cargo test --offline --lib providers::adapter::commandcode::browser_login` — FAIL to compile.

- [ ] **Step 3: Implement `browser_login.rs`**

Hand-roll the HTTP on a `tokio::net::TcpListener` — do **not** stand up a
second axum server for this; the surface is one path, two methods, and
the wizard runs before `axum::serve`.

- `bind_listener`: `for port in 5959..5969 { if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await { return Ok((l, port)); } }`, then `TcpListener::bind(("127.0.0.1", 0))` as the ephemeral fallback; propagate anything that isn't `AddrInUse`.
- Read the request head and body with a size cap (10 KiB, as in the reference) so a hostile local process can't balloon memory.
- CORS headers are set on **every** response (preflight and callback alike): `Access-Control-Allow-Origin` reflected from `["https://commandcode.ai", "https://staging.commandcode.ai", "http://localhost:3000"]` defaulting to `http://localhost:3000`; `Access-Control-Allow-Methods: POST, OPTIONS`; `Access-Control-Allow-Headers` echoing `Access-Control-Request-Headers` or `Content-Type`; `Access-Control-Allow-Private-Network: true`; `Content-Type: application/json`.
- `OPTIONS` → 204 and *keep listening* (the preflight is not the one request). The listener closes after the first *valid* `POST /callback`, an `error` payload, or the timeout.
- A body with a top-level `error` resolves as `LoginError::Denied(description)` (200 `{"success":true}` on the wire, so the browser tab shows success), matching the reference implementation.
- `wait()` wraps the accept loop in `tokio::time::timeout(AUTH_TIMEOUT, …)` with `AUTH_TIMEOUT` defaulting to 15s and overridable via `ROUTER_COMMANDCODE_AUTH_TIMEOUT_MS` (tests use this; env-var tests need the `static Mutex` guard).
- `open_in_browser` is strictly best-effort and must never return an error into the flow:
  ```rust
  #[cfg(target_os = "linux")]   let _ = std::process::Command::new("xdg-open").arg(url).spawn();
  #[cfg(target_os = "macos")]   let _ = std::process::Command::new("open").arg(url).spawn();
  #[cfg(target_os = "windows")] let _ = std::process::Command::new("cmd").args(["/c","start","",url]).spawn();
  ```
  The caller always prints the URL too — this is the accessibility/headless path, not a fallback of last resort.
- State validation lives in the *caller* (Task 6), matching the reference: `wait()` returns the raw callback including its `state`.

- [ ] **Step 4: Run to verify it passes**

Run (outside the sandbox): `cargo test --offline --lib providers::adapter::commandcode::browser_login` — PASS, all 11.

- [ ] **Step 5: Commit**

```bash
git add src/providers/adapter/commandcode/browser_login.rs src/providers/adapter/commandcode/mod.rs
git commit -m "feat(commandcode): one-shot local browser-login callback listener

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 6: Onboarding wizard — "Command Code" provider kind

Add the third choice to the wizard's provider-kind prompt and the
`add_commandcode_provider` step behind it.

**Files:**
- Modify: `src/onboarding.rs` (the `Select` at ~line 690, the `match kind` at ~line 697, a new `add_commandcode_provider` next to `add_codex_provider` at ~line 418, `mod tests`)
- Test: `cargo test --offline --lib onboarding`; manual verification per `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md`
- Modify: `docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md` (add a Command Code path to the checklist)

**Interfaces:**
- Consumes: Task 1's kind, Task 4's discovery, Task 5's `browser_login`.
- Produces: `pub async fn add_commandcode_provider(db, http) -> anyhow::Result<Provider>`, plus a `pub async fn store_commandcode_key(db, provider_id, key) -> Result<(), AppError>` helper that Task 7's admin endpoint reuses.

- [ ] **Step 1: Write the failing tests**

`onboarding.rs`'s prompt paths aren't `cargo test`-covered (it's a thin
`dialoguer` front end), so the unit test targets the non-interactive
helper only:

- `store_commandcode_key_writes_access_and_refresh_and_a_far_future_expiry` — against a temp SQLite pool with a provider row: after the call, `get_oauth_state` returns `access_token == refresh_token == the key`, `id_token.is_none()`, `provider_data == {}`, and `access_expires_at > Utc::now() + Duration::days(3000)`. Also assert `providers.api_key` is still `NULL`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding` — FAIL to compile.

- [ ] **Step 3: Implement**

`store_commandcode_key` is a two-liner over the existing generic query —
do not add a new storage path:

```rust
queries::upsert_oauth_tokens(
    db, provider_id, Some(key), Some(key), None,
    Some(Utc::now() + chrono::Duration::days(3650)),
    &serde_json::json!({}),
).await
```

`add_commandcode_provider` follows `add_codex_provider`'s shape exactly:
prompt for a name (used as the id), prompt for the wire format (same two
`Select` items, default Anthropic), insert the `Provider` with
`kind: OauthCommandCode`, `api_key: None`, `base_url: None`,
`upstream_model: PENDING_MODEL`, then:

1. `let (listener, port) = browser_login::bind_listener().await` — on `Err`, skip straight to the paste prompt.
2. **Spawn the listener task first**, then print the URL and call `open_in_browser`. This ordering matters: `run_wizard` is invoked from `main.rs` (~lines 106-110) inside the server's tokio runtime *before* `axum::serve`, so if the blocking `dialoguer` paste prompt ran on the same thread the listener is waiting on, the two deadlock. Run any `dialoguer` prompt via `tokio::task::spawn_blocking`.
3. On `Ok(cb)`: `if cb.state != issued_state { bail!("state token mismatch — authentication may have been tampered with") }`, then `store_commandcode_key`.
4. On `LoginError::Timeout` (or a bind failure): print "Automatic transfer failed or timed out." and prompt (in `spawn_blocking`) "Paste your Command Code API key", `sanitize_api_key` the result, reject empty, `store_commandcode_key`.
5. Then discover models instead of Codex's blind candidate probe: call `providers::routes::discover_and_cache_models`-equivalent logic (or the same `fetch_commandcode_models` helper) and, if it returns models, offer them via a `Select`; fall back to a free-text `Input` if discovery fails. Persist the chosen model as `upstream_model`.

Then extend the kind `Select` to three items and the `match` to three arms:

```rust
.items(["passthrough (OpenAI/Anthropic-compatible API key)",
        "Codex OAuth (ChatGPT account)",
        "Command Code (commandcode.ai browser login)"])
…
0 => add_passthrough_provider(db).await?,
1 => add_codex_provider(db, http).await?,
_ => add_commandcode_provider(db, http).await?,
```

(Note the existing `_ =>` arm currently catches index 1; it must become an explicit `1 =>` or the new option is unreachable.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding` — PASS.
Then run the wizard for real: `cargo run --offline -- setup` against a temp DB, pick "Command Code", and confirm the browser opens (or the URL prints), a successful login stores the key, and Ctrl-C'ing the browser step lands you at the paste prompt after ~15s rather than hanging forever.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs docs/superpowers/plans/2026-07-26-onboarding-wizard-smoke.md
git commit -m "feat(onboarding): Command Code provider wizard step

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 7: Admin API + UI — paste-the-key, kind dropdown, discovery filters

The admin UI gets the kind in its dropdown, admission to the two
discovery filters, and a plain API-key field. Deliberately **no**
browser-login button: the admin UI is a server-hosted SPA, so a local
listener only works when browser and server share a machine.

**Files:**
- Modify: `src/providers/oauth_routes.rs` (new `POST /admin/providers/:id/commandcode/key` route + handler)
- Modify: `frontend/src/pages/Providers.tsx` (kind `<select>` ~lines 284-286; a `CommandCodeKeyPanel` alongside `CodexOAuthPanel` at ~line 312)
- Modify: `frontend/src/pages/Settings.tsx` (`discoverableProviders` filter ~line 53; the empty-state copy ~line 267)
- Test: `cargo test --offline --test admin_providers`; `npm --prefix frontend test` (or the repo's existing frontend test command) for the React changes

**Interfaces:**
- Consumes: Task 6's `store_commandcode_key`, Task 1's kind, Task 4's discovery.
- Produces: `POST /admin/providers/:id/commandcode/key` with body `{"api_key":"…"}` → `{"ok":true}`.

- [ ] **Step 1: Write the failing tests**

Integration, in `tests/admin_providers.rs` (uses `tests/common::spawn_app`, which binds a socket — sandbox-blocked, see the global constraints):

1. `commandcode_key_endpoint_stores_the_key` — create a provider with `kind: "oauth_command_code"`, POST the key, then assert via a direct DB read that `provider_oauth_state.access_token == refresh_token == key` and that `GET /admin/providers/:id` still shows `api_key: null`.
2. `commandcode_key_endpoint_rejects_a_non_commandcode_provider` — 400 for a `passthrough` provider, mirroring the `oauth/start` guard's `"provider is not oauth_codex"` message shape.
3. `commandcode_key_endpoint_rejects_an_empty_key` — 400.
4. `create_provider_accepts_the_new_kind` — `POST /admin/providers` with `"kind":"oauth_command_code"` → 201, and the round-tripped `kind` in the response is `"oauth_command_code"`.

Frontend: extend the existing `Providers.tsx`/`Settings.tsx` test files to assert the dropdown contains an `oauth_command_code` option and that a Command Code provider appears in the Settings discovery list.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --test admin_providers` — FAIL (404 on the new route).

- [ ] **Step 3: Implement**

In `oauth_routes.rs`, add to `routes()`:

```rust
.route("/admin/providers/:id/commandcode/key", post(set_commandcode_key))
```

**axum 0.7:** `:id`, not `{id}` — the wrong syntax silently 404s instead
of failing to compile. Grep the file for `{[a-zA-Z_]*}` in route strings
before committing.

The handler loads the provider, rejects any kind other than
`OauthCommandCode` with `AppError::BadRequest`, rejects an empty/whitespace
key, calls `store_commandcode_key`, then `reload_snapshot(&s)` (matching
what `complete` does), and returns `{"ok":true}`. The key must never be
echoed back or logged.

Frontend:
- `Providers.tsx`: add `<option value="oauth_command_code">oauth_command_code</option>`; render a `CommandCodeKeyPanel` (a single password-type input + Save button POSTing to the new endpoint, modelled on `CodexOAuthPanel`) when `editing && form.kind === "oauth_command_code"`.
- `Settings.tsx`: change the filter to `providers.filter((p) => p.kind === "passthrough" || p.kind === "oauth_command_code")` and update the empty-state copy (it currently says "No passthrough providers to check (Codex OAuth providers have no discoverable model list)" — only Codex lacks discovery now).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --test admin_providers` (outside the sandbox — `spawn_app` binds a socket) and the frontend test command. Then build the UI and click through: create a Command Code provider, paste a key, and confirm the Settings page's "Check providers for available models" lists its models.

- [ ] **Step 5: Commit**

```bash
git add src/providers/oauth_routes.rs frontend/src tests/admin_providers.rs
git commit -m "feat(admin): Command Code kind, key endpoint, discovery admission

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 8: End-to-end integration test and documentation

Prove a real proxy request survives the whole path, and record the new
provider in the docs the next contributor reads.

**Files:**
- Create: `tests/commandcode_proxy.rs`
- Modify: `README.md` (provider list / setup section)
- Modify: `CLAUDE.md` (the "no per-provider hardcoded code anywhere except the single Codex adapter" claim is now false — there are two adapters)
- Modify: `docs/superpowers/specs/2026-07-25-1router-design.md` (same one-line correction, if it repeats the claim)
- Test: `cargo test --offline --test commandcode_proxy`

**Interfaces:**
- Consumes: everything above.
- Produces: no new code surface.

> **⚠ Codex sandbox note:** `tests/common::spawn_app` binds a real socket, so this whole file is sandbox-blocked. Re-run it outside the sandbox.

- [ ] **Step 1: Write the failing tests**

In `tests/commandcode_proxy.rs`, using `wiremock` as the fake Command Code
upstream (point the adapter at it via the same env override introduced in
Task 4, extended to cover `GENERATE_URL` — e.g.
`ROUTER_COMMANDCODE_BASE_URL`, defaulting to `https://api.commandcode.ai`):

1. `openai_wire_streaming_end_to_end` — `POST /v1/chat/completions` with `stream:true` against a pool backed by a Command Code provider; upstream replies with canned NDJSON; assert the client sees framed `data: {…}` chunks ending in `data: [DONE]`, and that the request wiremock received hit `/alpha/generate` with `Authorization: Bearer` + the five fixed headers.
2. `anthropic_wire_streaming_end_to_end` — `POST /v1/messages` against an `anthropic`-wire pool; assert `event: message_start` … `event: message_stop`.
3. `non_streaming_aggregates` — `stream:false` → a single `chat.completion` JSON body.
4. `upstream_429_cools_the_provider_and_fails_over` — a second passthrough provider at lower priority serves the request, proving `classify_error` is delegating to `proxy::backoff` and the adapter is not swallowing retries itself.
5. `a_401_marks_the_provider_misconfigured_rather_than_attempting_a_refresh` — assert via `GET /admin/providers/:id/state` that `status == "misconfigured"` and that no refresh was attempted (no second upstream call).

- [ ] **Step 2: Run to verify it fails**

Run (outside the sandbox): `cargo test --offline --test commandcode_proxy` — FAIL.

- [ ] **Step 3: Make the base URL overridable and fix the docs**

The only production change here is threading the env override through
`commandcode::adapter` (same pattern as Task 4's models URL: read once,
fall back to the const). Everything else is documentation:

- `CLAUDE.md`: the summary line "config-only OpenAI/Anthropic-compatible passthrough providers, plus a Codex OAuth adapter" and the Global-Constraint-style claim "No per-provider hardcoded code anywhere except the single Codex adapter" both need to name both adapters. Add a bullet under Onboarding noting the Command Code wizard path, and a bullet under the Codex-sandbox-limitations list noting that `providers::adapter::commandcode::browser_login`'s tests bind a listener.
- `CLAUDE.md`'s doc index: add this plan and its design spec.
- `README.md`: list Command Code among the supported provider kinds and describe the two credential paths (CLI browser login, admin paste).

- [ ] **Step 4: Run to verify it passes**

Run (outside the sandbox): `cargo test --offline` — the full suite, PASS. Confirm no pre-existing test regressed, in particular `codex_oauth`, `proxy_failover`, `proxy_streaming`, `admin_export_import` (which round-trips `kind` through JSON) and `provider_auto_discovery`.

- [ ] **Step 5: Commit**

```bash
git add tests/commandcode_proxy.rs src/providers/adapter/commandcode README.md CLAUDE.md docs/superpowers/specs/2026-07-25-1router-design.md
git commit -m "test(commandcode): end-to-end proxy coverage; docs

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage** — every section of `docs/superpowers/specs/2026-08-04-commandcode-provider-design.md` maps to a task:

| Spec section | Task |
|---|---|
| Why a bespoke adapter (request envelope, NDJSON response) | Tasks 2, 3 |
| Reuse: `claude_bridge` in both directions | Task 3 (Steps 1.4, 1.6) |
| Reuse: no inner retry loop, `proxy::backoff` owns it | Task 3 Step 3 (`classify_error`), Task 8 test 4 |
| Reuse: `provider_oauth_state`, no migration | Task 1 (Global Constraints), Task 6 Step 3 |
| Non-refreshing key model (10-year expiry, `api_key` NULL, no refresh gates) | Task 6 Step 1/3, Task 3 Step 1.8, Task 8 test 5 |
| SSE framing contract | Task 2 test 8, Task 3 test 6 |
| Browser login (ports, CORS, callback, CSRF, timeout, deadlock note) | Task 5, Task 6 Step 3 |
| Model discovery (fixed URL, unauthenticated, shared parser) | Task 4 |
| Admin UI scope (no browser button) | Task 7 |
| Out of scope | Global Constraints |

**2. Placeholder scan** — no `TBD`, no "add appropriate …". Every URL, header name, header value, JSON key and enum spelling is given literally.

**3. Name consistency across tasks** — `ProviderKind::OauthCommandCode` / DB+JSON value `oauth_command_code` appears identically in Tasks 1, 3, 4, 6, 7, 8. `DEFAULT_MODELS_URL` is defined in Task 3 and consumed in Task 4. `store_commandcode_key` is defined in Task 6 and consumed in Task 7. `browser_login::{bind_listener, AuthCallback, sanitize_api_key}` are defined in Task 5 and consumed in Task 6. The env overrides `ROUTER_COMMANDCODE_MODELS_URL` (Task 4), `ROUTER_COMMANDCODE_AUTH_TIMEOUT_MS` (Task 5) and `ROUTER_COMMANDCODE_BASE_URL` (Task 8) are each introduced once and only used in tests.

**4. Sequencing** — Task 1 must land first (every other task matches on the variant). Tasks 2 and 5 are leaf-parallel afterwards. Task 3 joins 1+2. Task 4 needs 3 (for `DEFAULT_MODELS_URL`). Task 6 joins 4+5. Task 7 needs 6. Task 8 joins everything.

### Critical Files for Implementation
- /home/ducph/duc/1router/src/providers/adapter/codex/adapter.rs
- /home/ducph/duc/1router/src/providers/adapter/mod.rs
- /home/ducph/duc/1router/src/core/model.rs
- /home/ducph/duc/1router/src/providers/routes.rs
- /home/ducph/duc/1router/src/onboarding.rs
