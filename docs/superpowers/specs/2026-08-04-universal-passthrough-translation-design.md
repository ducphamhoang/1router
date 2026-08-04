# Universal passthrough wire-format translation — Design

## Goal

Let a client hit either `/v1/chat/completions` (OpenAI Chat Completions
shape) or `/v1/messages` (Anthropic Messages shape) against **any**
provider — including plain config-only `passthrough` providers — and get a
correctly-shaped response, regardless of which wire format that provider's
own upstream actually speaks. Today this already works for the two
adapter-backed provider kinds (`oauth_codex`, `oauth_command_code`); this
phase extends the same guarantee to `passthrough`.

## Current state (why this doesn't already work)

`PassthroughAdapter::build_request` (`src/providers/adapter/passthrough.rs`)
forwards `client_body` to `base_url` essentially unchanged (it only swaps in
`upstream_model` and picks the auth header style from `provider.wire_format`).
`transform_response` streams the upstream response's bytes back untouched.
There is no reshaping in either direction — the client's request body must
already be in the shape the upstream expects.

Two guards currently enforce this at the boundary rather than translating:

- `Provider::supports_wire` (`src/core/model.rs`) returns `true`
  unconditionally for `OauthCodex`/`OauthCommandCode` (they translate) but
  `self.wire_format == w` for `Passthrough` (they don't).
- `PUT /admin/pools/:id/members` (`src/pools/routes.rs`) rejects adding a
  passthrough provider to a pool whose `wire_format` doesn't match the
  provider's own, with the explicit message *"passthrough providers cannot
  translate between formats"*.
- `select_direct_provider` (`src/pools/select.rs`) applies the same
  `supports_wire` check for `<provider_id>/<model>` direct addressing.

A **pool**'s own `wire_format` field is a separate, orthogonal constraint
(`select()` rejects a request outright if the pool's `wire_format` doesn't
match the route hit) and is **out of scope** for this phase — it stays
exactly as-is. What changes is only whether a `passthrough` provider is
*eligible* to join/be addressed under a wire format that differs from its
own `provider.wire_format`.

## Reuse, not new machinery

`src/providers/adapter/codex/claude_bridge.rs` already contains a full,
tested Anthropic Messages ⇄ OpenAI Chat Completions bridge, but only in the
direction the two existing adapters need (their upstream integrations are
always OpenAI-shape internally, so `claude_bridge` only ever converts
*client* Anthropic ⇄ *internal* OpenAI):

- `claude_to_openai_request` (request, Anthropic → OpenAI)
- `openai_json_to_claude_message` / `convert_openai_sse_to_claude_sse`
  (response, OpenAI → Anthropic, non-streaming and streaming)

A `passthrough` provider's own `wire_format` can be *either* value (it's a
real Anthropic-compatible endpoint just as often as an OpenAI-compatible
one — see the DeepSeek `/anthropic/v1/messages` preset in `onboarding.rs`),
so this phase adds the missing **reverse** direction to the same module:

- `openai_to_claude_request` (request, OpenAI → Anthropic)
- `claude_json_to_openai_message` / `convert_claude_sse_to_openai_sse`
  (response, Anthropic → OpenAI, non-streaming and streaming)

This keeps every OpenAI⇄Anthropic shape-mapping decision (tool call
encoding, image blocks, stop-reason/finish-reason mapping, usage-field
renaming) in one file, reused by three call sites (`codex`, `commandcode`,
and now `passthrough`), instead of forking the logic.

### The new byte-reframing requirement

`convert_openai_sse_to_claude_sse` assumes each stream item it receives is
already exactly one complete `\n\n`-terminated SSE block — true for its
existing callers because they run *after* `codex::transform::convert_sse_stream`
/ `commandcode::transform::convert_ndjson_stream`, which already reframe
arbitrarily-chunked upstream bytes into discrete blocks as part of their own
envelope translation. A plain passthrough provider has no such intermediate
step: `reqwest`'s `bytes_stream()` hands back whatever chunking the network
gave it, with no guarantee it aligns to SSE block boundaries.

So this phase adds one small, format-agnostic utility,
`claude_bridge::reframe_sse_blocks`, that buffers arbitrarily-chunked bytes
and yields one item per complete block. `PassthroughAdapter` wraps the raw
upstream stream with it before handing off to either converter. The
existing two converters are left untouched (signature and behavior) to
avoid any risk to their existing test coverage — reframing is the caller's
job now, symmetric on both directions.

## What changes in `PassthroughAdapter`

`PassthroughAdapter` gains a `client_wire: WireFormat` field, threaded
through by `adapter_for_wire` (which already receives it — today it's
simply discarded for the `Passthrough` arm).

- `build_request`: if `client_wire != provider.wire_format`, translate
  `client_body` through `claude_bridge::{claude_to_openai_request,
  openai_to_claude_request}` (picking direction by `client_wire`) before
  injecting `upstream_model` and building the request. If they match, behave
  exactly as today (verbatim forward).
- `transform_response`: if `client_wire != provider.wire_format`, translate
  the upstream response through
  `claude_bridge::{reframe_sse_blocks + convert_openai_sse_to_claude_sse}` or
  `claude_bridge::{reframe_sse_blocks + convert_claude_sse_to_openai_sse}`
  for streaming, or the non-streaming JSON converters for a buffered
  response, picking direction by which side is the client vs. the upstream.
  If they match, stream bytes through untouched as today.

## What changes elsewhere

- `Provider::supports_wire` is deleted — once `PassthroughAdapter`
  translates, **every** provider kind supports every wire format
  unconditionally, which makes the method a constant `true` not worth
  keeping as a method.
- The `PUT /admin/pools/:id/members` guard in `src/pools/routes.rs` that
  called it is deleted outright — a passthrough provider can now join a
  pool of either `wire_format`.
- `select_direct_provider` in `src/pools/select.rs` drops its
  `supports_wire` check for the same reason — `<provider_id>/<model>` direct
  addressing now works against any provider from either client route.
- Admin UI copy that describes this restriction (if any) gets updated;
  `README.md`'s "Wire formats" section is rewritten to state the guarantee
  is now universal, not exception-cased to the two adapter kinds.

## Explicitly out of scope

- The pool-level `wire_format` lock (`select()`'s
  `pwm.pool.wire_format != wire` check) is untouched — a named **pool**
  still answers exactly one client route, same as today for every provider
  kind. Serving both routes from one provider still requires either direct
  addressing or two separate pool rows, exactly as it already does for
  Codex/Command Code.
- Anthropic "thinking"/extended-thinking blocks and OpenAI reasoning-model
  fields are not modeled in either direction — same gap that already exists
  in the Codex/Command Code direction this reuses.
- No new provider kind, no schema/migration changes, no new admin
  endpoints — this is a translation-layer change inside the existing
  `passthrough` kind.
