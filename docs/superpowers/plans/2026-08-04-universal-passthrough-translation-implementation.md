# Universal passthrough wire-format translation — Implementation plan (v0.3.2)

Design: `docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`

## T1 — Reverse request translation: OpenAI → Anthropic

In `src/providers/adapter/codex/claude_bridge.rs`, add
`pub fn openai_to_claude_request(body: &Value) -> Value`, mirroring
`claude_to_openai_request` in reverse:

- Pull `system`-role messages out of `messages` into Anthropic's top-level
  `system` field (joined with `\n` if more than one).
- Map each remaining message: `tool`-role → a `user` message with one
  `tool_result` block (`tool_use_id` from `tool_call_id`); `assistant` with
  `tool_calls` → `assistant` content blocks (`text` block for any prose +
  one `tool_use` block per call, `input` parsed from the `arguments` JSON
  string); everything else → plain string or, for array content
  (`image_url` parts), `image` blocks (base64 `data:` URIs → `source.type
  = "base64"`, anything else → `source.type = "url"`).
- Copy `model`/`stream`/`temperature` through as-is. Anthropic requires
  `max_tokens`; default to `4096` if the OpenAI body omitted it.
- Map `tools` (`{type:"function",function:{name,description,parameters}}`)
  → Anthropic's `{name,description,input_schema}`, and `tool_choice` the
  mirror of the existing `convert_tool_choice`.

Unit tests alongside the existing `claude_to_openai_request` tests:
system message extraction, tool_use/tool_result round trip, tools/tool_choice
mapping, missing `max_tokens` defaulting.

## T2 — Reverse response translation: Anthropic → OpenAI (non-streaming)

Same file: `pub fn claude_json_to_openai_message(value: &Value) -> Value`,
mirroring `openai_json_to_claude_message`. Concatenate `text` content
blocks into `choices[0].message.content`; `tool_use` blocks →
`message.tool_calls`; `stop_reason` → `finish_reason` (reverse of
`finish_to_stop_reason`); usage: `input_tokens + cache_read_input_tokens` →
`prompt_tokens`, `output_tokens` → `completion_tokens`, cached count →
`usage.prompt_tokens_details.cached_tokens` (mirrors the subtraction
`usage_from` already does the other way).

Tests: text-only message, tool_use message, cache-read usage round trip.

## T3 — Reverse streaming translation: Anthropic SSE → OpenAI SSE

Same file: an `OpenAiStreamState` (mirrors `ClaudeStreamState`) tracking
per-content-block-index state (text vs. tool_use, and the tool's OpenAI
`tool_calls[].index`), plus:

- A small generic parser for one `event: X\ndata: Y` block →
  `(event_type, data: Value)` (Anthropic SSE framing, distinct from the
  bare `data: {...}` OpenAI framing `openai_chunk_to_claude_events` already
  parses).
- `pub fn claude_event_to_openai_chunk(event_type: &str, data: &Value,
  state: &mut OpenAiStreamState) -> Option<Value>`: `content_block_start`
  opens a text/tool_use block; `content_block_delta` emits a
  `chat.completion.chunk` with `delta.content` (text) or
  `delta.tool_calls[0].{index,id,function.name,function.arguments}`
  (tool_use, id/name only on the block's first delta, matching how OpenAI
  itself streams); `message_delta` emits the finish chunk (`finish_reason`
  + usage); `message_start`/`content_block_stop`/`message_stop`/`ping`
  produce no chunk (message_stop is handled by the stream wrapper below,
  which appends `[DONE]`).
- `pub fn convert_claude_sse_to_openai_sse<S, E>(upstream: S) -> impl
  Stream<...>`, the mirror of `convert_openai_sse_to_claude_sse`: same
  "already one block per item" assumption, appends `data: [DONE]\n\n` once
  upstream ends (Anthropic streams don't have a `[DONE]` marker; OpenAI's
  do).

Tests: mirror the existing `stream_events_*`/`convert_openai_sse_to_claude_sse_*`
tests one-for-one in the opposite direction (message_start → first text
delta, tool_use block streaming, finish/usage mapping, full round-trip
through the stream wrapper).

## T4 — Byte reframing utility

Same file: `pub fn reframe_sse_blocks<S, E>(upstream: S) -> impl
Stream<Item = Result<Bytes, E>>` — buffers a `Bytes` stream and yields one
item per complete `\n\n`-terminated block (flushing any non-terminated tail
once the upstream stream ends), independent of block content/format so it
works ahead of either converter. Test: a block split across two/three
input chunks reassembles into one output item; multiple blocks in one input
chunk split into multiple output items.

## T5 — Wire it into `PassthroughAdapter`

`src/providers/adapter/passthrough.rs`:

- Add `client_wire: WireFormat` field + constructor param.
  `adapter_for_wire` (`src/providers/adapter/mod.rs`) passes its existing
  `client_wire` argument through to `PassthroughAdapter::new` instead of
  discarding it.
- `build_request`: after parsing `client_body` to `Value`, if
  `self.client_wire != self.provider.wire_format`, run it through
  `claude_bridge::claude_to_openai_request` (client Anthropic → provider
  OpenAI) or `claude_bridge::openai_to_claude_request` (client OpenAI →
  provider Anthropic) before the existing `upstream_model` injection. No
  change when they match.
- `transform_response`: if formats match, behave exactly as today
  (untouched byte/status/header passthrough). If they differ and
  `client_wanted_stream`, wrap `upstream.bytes_stream()` in
  `claude_bridge::reframe_sse_blocks` then the matching converter
  (`convert_openai_sse_to_claude_sse` or `convert_claude_sse_to_openai_sse`)
  and stream that back with `content-type: text/event-stream`. If they
  differ and the client didn't ask to stream, buffer the upstream body,
  parse it as JSON, and run it through `claude_json_to_openai_message` /
  `openai_json_to_claude_message`, returning it as a normal JSON response
  (status forwarded, but headers rebuilt rather than copied — the upstream's
  `content-length`/`content-type` no longer describe the translated body).

Tests: extend `passthrough.rs`'s existing adapter tests with one
request-build case and one response-transform case per direction
(4 total: OpenAI client → Anthropic provider and reverse, request and
response), plus a streaming case per direction using a fake chunked
`bytes_stream`.

## T6 — Remove the now-dead compatibility gates

- `src/core/model.rs`: delete `Provider::supports_wire` and its test
  (`provider_supports_wire_depends_on_kind`) — with `PassthroughAdapter`
  translating, every kind supports every wire format unconditionally.
- `src/pools/routes.rs`: delete the `put_member` guard block that called
  `supports_wire` (lines ~85-90) and its rejection message.
- `src/pools/select.rs`: delete the `supports_wire` check in
  `select_direct_provider`. Update
  `direct_provider_addressing_rejects_a_wire_format_mismatch` — that
  behavior no longer exists, so replace the test with one asserting the
  opposite (`Some`, translation now applies) or delete it if
  `direct_codex_provider_addressing_supports_both_wire_formats` already
  covers the assertion generically enough once passthrough is added to it.

## T7 — Admin UI copy + docs

- Grep the frontend for any string mentioning passthrough/wire-format
  restrictions (the Pools/Providers pages) and update or remove it.
- `README.md` "Wire formats" section: replace "Ordinary passthrough
  providers are pure config and speak whichever one format their
  `wire_format` says - a pool must stay homogeneous, so a passthrough
  provider can only join a pool matching its own format" with the
  universal-translation framing; fold the "Codex/Command Code are the
  exceptions" paragraph into "every provider kind translates now", keeping
  the still-true pool-locked-to-one-route caveat.
- `Cargo.toml`: bump `version` to `0.3.2`.
- `CLAUDE.md`: no plan/spec cross-reference needed beyond what's already
  covered by "Progress ledger" conventions; leave as-is unless a new
  gotcha is discovered during implementation (record it there if so).

## T8 — Full verification

`cargo build --offline && cargo test --offline` (lib + all integration
files). Manually re-run the two curl probes from the smoke test this
feature was requested from: a passthrough Anthropic-only provider (e.g. the
DeepSeek `/anthropic/v1/messages` preset) called via
`/v1/chat/completions`, and a passthrough OpenAI-only provider called via
`/v1/messages`, both non-streaming and streaming, against a pool created
with the matching-to-the-route `wire_format` (recall: the *pool's* format
still has to match the route per T's "explicitly out of scope" — what's
newly possible is a provider whose own upstream format differs from the
pool/route it's serving).
