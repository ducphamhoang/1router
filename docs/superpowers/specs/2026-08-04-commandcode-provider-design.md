# Command Code provider (commandcode.ai) — Design

## Goal

Let a 1router operator point a pool at a Command Code (commandcode.ai)
account and have both OpenAI-wire clients (`POST /v1/chat/completions`)
and Anthropic-wire clients (`POST /v1/messages`, i.e. Claude Code) work
against it, with the credential obtained through a browser login from
`1router setup` (or pasted into the admin UI).

## Why a bespoke adapter, not a config-only passthrough

`PassthroughAdapter` works only when the upstream speaks OpenAI Chat
Completions or Anthropic Messages on the wire. Command Code speaks
neither:

- **Request**: `POST https://api.commandcode.ai/alpha/generate` with a
  proprietary envelope — `{config:{workingDir,date,environment,…},
  memory, taste, skills, params:{model,messages,tools,system,max_tokens,
  temperature,stream}, threadId}` — plus five fixed non-standard headers
  (`x-command-code-version`, `x-cli-environment`, `x-project-slug`,
  `x-taste-learning`, `x-co-flag`). `params.messages` is an
  AI-SDK-flavoured shape (`{type:"tool-call",toolCallId,toolName,input}`,
  `{type:"reasoning",text}`, tool results as
  `{type:"tool-result",output:{type:"text"|"error-text",value}}`), not
  OpenAI's `{role:"tool",tool_call_id,content}`.
- **Response**: NDJSON events (`text-delta`, `reasoning-start/delta/end`,
  `tool-call`, `tool-result`, `finish`, `error`), not SSE
  `chat.completion.chunk` blocks. Upstream always streams regardless of
  what the client asked for.

That is exactly the situation the existing Codex adapter exists for, so
Command Code becomes the second entry in `providers/adapter/`:
`ProviderKind::OauthCommandCode` + `src/providers/adapter/commandcode/`.
Everything outside that module stays generic; the only per-kind code
added elsewhere is a handful of `match`/gate arms.

## Reuse, not new machinery

- **Anthropic wire**: reuse `codex::claude_bridge` as-is —
  `claude_to_openai_request` for the inbound leg,
  `convert_openai_sse_to_claude_sse` / `openai_json_to_claude_message`
  for the outbound leg. The adapter's own job is therefore narrowed to
  *OpenAI-shape ⇄ Command-Code-shape*, in both directions. This is what
  makes the SSE framing requirement below load-bearing.
- **Retries/backoff**: the reference JS implementation carries its own
  429/5xx retry loop with `Retry-After` parsing. That is *not* ported —
  `proxy::backoff::classify` plus the pool failover loop in
  `proxy::flow` already own that policy for every provider, and a second
  inner loop would double-count cooldowns.
- **Credential storage**: `provider_oauth_state` / `OAuthState`, unchanged.
  The core design spec already states this table is "present for any
  `oauth_*` kind, not Codex-specific … so a second adapter needs no
  schema change", and `proxy::flow::credentials_for` +
  `queries::upsert_oauth_tokens` are already kind-agnostic and
  provider-id-keyed. **No migration is needed** (`providers.kind` is a
  plain `TEXT` column with no `CHECK` constraint).

## The non-refreshing key model

A Command Code API key never expires. The browser flow yields a key, not
an OAuth token pair, so:

- `upsert_oauth_tokens(db, provider_id, Some(key), Some(key), None,
  Some(now + 10 years), &json!({}))` — access == refresh == the key,
  far-future expiry, empty `provider_data`.
- `Provider.api_key` stays `NULL` (same rule as `oauth_codex`); the
  adapter reads `creds.access_token`.
- `needs_refresh` → always `false`; `refresh_credentials` is a defensive
  no-op that echoes the credentials back.
- The background refresh loop (`providers::refresh_task`) and the
  reactive `AuthExpired` recovery branch (`proxy::flow`) stay
  `OauthCodex`-only. A Command Code 401 is therefore surfaced as
  *misconfigured* (re-login required), which is correct — there is
  nothing to refresh.

## SSE framing (the one easy thing to get wrong)

`transform.rs` must emit **properly framed** `data: {…}\n\n` blocks
terminated by `data: [DONE]\n\n`, exactly like
`codex::transform::render_chunk` / `SSE_DONE`.
`claude_bridge::convert_openai_sse_to_claude_sse` parses one complete
block per stream item; handing it raw NDJSON produces a silently empty
Anthropic stream rather than an error. Inbound parsing is deliberately
tolerant of both raw NDJSON lines and `data:`-prefixed lines (and skips
`:`/`event:` comments and `[DONE]`), mirroring the reference
implementation's `parseStreamEventLine`.

## Browser login

`1router setup` binds `127.0.0.1:5959`, scanning up to 10 ports and then
an OS-ephemeral port, opens
`https://commandcode.ai/studio/auth/cli?callback=<urlencoded
http://localhost:{port}/callback>&state=<random>` best-effort via
`std::process::Command` (`xdg-open`/`open`/`cmd /c start` — deliberately
*not* a new crate, so no online `cargo fetch` is required), always also
prints the URL, and serves exactly one request: an `OPTIONS` preflight
(204, origin reflected from an allowlist of `https://commandcode.ai`,
`https://staging.commandcode.ai`, `http://localhost:3000`;
`Access-Control-Allow-Private-Network: true` for Chrome's PNA check) and
then `POST /callback` with `{apiKey,state,userId,userName,keyName}` — all
five required non-empty, else 400. The caller checks `state` (CSRF). On a
15s timeout or a bind failure, it falls back to a `dialoguer` paste
prompt.

Concurrency: the first-boot wizard runs inside the server's tokio runtime
before `axum::serve`, so the listener is spawned as a task first and the
blocking paste prompt runs via `spawn_blocking` — otherwise the fallback
prompt and the listener deadlock on the same thread.

## Model discovery

`GET https://api.commandcode.ai/provider/v1/models`, **unauthenticated**,
returning `{object:"list", data:[{id,name,context_length}]}`. The
existing `derive_models_url` (swap the last path segment of `base_url`)
cannot produce this from `/alpha/generate`, and the existing fetch path
always attaches provider auth. Following the precedent Codex set —
hardcoding its fixed upstream URL as a module `const` rather than
deriving it — the adapter exposes `DEFAULT_MODELS_URL`, and
`providers::routes`' discovery functions branch on the kind to use a
dedicated unauthenticated fetch. The response parser is unchanged:
`data[].id` is all that is read, extra fields are ignored.

## Admin UI scope

The admin UI gets "Command Code" in the kind dropdown, admission to the
model-discovery panels, and a plain "paste your API key" field that
persists through the same `upsert_oauth_tokens` path. It deliberately
does **not** get a browser-login button: the admin UI is a server-hosted
SPA, so a local listener only works when browser and server share a
machine — an assumption that holds for the CLI wizard and not for the
web UI.

## Out of scope (v1)

- Cost accounting from `finish.totalUsage` beyond mapping it into the
  OpenAI `usage` object.
- Any refresh/rotation of Command Code keys.
- `tool-result` events echoed by upstream (parsed and ignored, as in the
  reference implementation).
- Browser-login from the admin UI.
