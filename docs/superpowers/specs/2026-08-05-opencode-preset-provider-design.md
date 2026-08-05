# OpenCode as a preset provider — Design

## Goal

Let a 1router operator pick "OpenCode" from the existing provider-template
dropdown (CLI wizard and admin UI) instead of hand-typing OpenCode Go's base
URL, wire format, and a model id. No new adapter, no new `ProviderKind`, no
migration — OpenCode Go speaks the two wire formats 1router already
understands natively.

## Reference

`/home/ducph/duc/9router-reference` (a sibling Node/Next.js gateway,
read-only) ships two OpenCode entries under
`open-sse/providers/registry/`:

- `opencode.js` — **OpenCode Free**: `noAuth: true`, hardcoded
  `Authorization: Bearer public` + `x-opencode-client: desktop` header,
  base `https://opencode.ai/zen/v1`, model list fetched live (no static
  models). See "Out of scope" below for why this one doesn't fit v1.
- `opencode-go.js` — **OpenCode Go**: a $5/mo API-key subscription, base
  `https://opencode.ai/zen/go/v1`, 14 static models split across two wire
  shapes on the same account:
  - most models (`glm-5.2`, `glm-5.1`, `kimi-k2.7-code`, `kimi-k2.6`,
    `deepseek-v4-pro`, `deepseek-v4-flash`, `mimo-v2.5`, `mimo-v2.5-pro`) →
    `POST {base}/chat/completions`, `Authorization: Bearer {key}` — OpenAI
    Chat Completions, verbatim.
  - `minimax-m3`, `minimax-m2.7`, `minimax-m2.5`, `qwen3.7-max`,
    `qwen3.7-plus`, `qwen3.6-plus` → `POST {base}/messages`,
    `x-api-key: {key}` + `anthropic-version: 2023-06-01` — Anthropic
    Messages, verbatim (`open-sse/executors/opencode-go.js`'s
    `MESSAGES_FORMAT_MODELS` set and its `buildHeaders`/`buildUrl` split).

Both shapes are exactly what `PassthroughAdapter`
(`src/providers/adapter/http.rs`) already speaks — it picks
`Authorization: Bearer` vs. `x-api-key`+`anthropic-version` purely off
`provider.wire_format`, which is precisely the reference implementation's
per-model routing collapsed to per-provider-row routing. This is the same
shape 1router already solved once for DeepSeek (reachable via both an
OpenAI-compatible and an Anthropic-compatible endpoint) — see
`PROVIDER_TEMPLATES` in `src/onboarding.rs` and
`frontend/src/pages/Providers.tsx`.

## Design: two more `PROVIDER_TEMPLATES` entries, no backend code

Because a `Provider` row is one `wire_format` + one `base_url` + one
`upstream_model`, OpenCode Go's two wire shapes become two presets on the
same underlying account, exactly like the two DeepSeek entries:

| Label | wire_format | base_url | default upstream_model |
|---|---|---|---|
| OpenCode (OpenAI-compatible) | openai | `https://opencode.ai/zen/go/v1/chat/completions` | `kimi-k2.7-code` |
| OpenCode (Anthropic-compatible) | anthropic | `https://opencode.ai/zen/go/v1/messages` | `qwen3.7-max` |

An operator who wants both families of model adds the provider twice (once
per template) with the same API key, the same way they would for DeepSeek
today — no new concept for them to learn.

This requires **zero changes** to `src/providers/adapter/http.rs`,
`src/core/model.rs`, or `ProviderKind`: it's config data, not code. The
only files that change are the two `PROVIDER_TEMPLATES` arrays (kept in
sync per the existing "mirrors ... keep the two in sync" comment already
on both) plus the model-name suggestion lists.

## Model discovery comes for free

`derive_models_url` (`src/providers/routes.rs`) strips a trailing
`/chat/completions` or `/messages` and appends `/models` — for either
preset above that lands on `https://opencode.ai/zen/go/v1/models`, and the
existing auth-header logic (`Bearer` for OpenAI wire, `x-api-key` for
Anthropic wire) is exactly what the account's own key expects. No dedicated
fetch path is needed (contrast with Command Code, which needed one because
its models endpoint is unauthenticated and off a completely different
path shape than its `/alpha/generate` generate endpoint).

This is unverified against the live endpoint (no network access from this
environment) — Task 1 below calls it out as a manual smoke check, not an
assumption baked into the plan.

## Model-name suggestions

`frontend/src/pages/Pools.tsx` keeps static `<datalist>` suggestions per
wire format, used until "Fetch models" replaces them with a live list.
Unlike `DEEPSEEK_MODEL_SUGGESTIONS` (genuinely reachable through either
wire format, so it's spread into both lists), OpenCode Go's models are
*not* interchangeable across wire formats — each model id only answers on
the endpoint the reference implementation routes it to. So this is a
single `OPENCODE_MODEL_SUGGESTIONS`-shaped split: the eight
`chat/completions` models go into `OPENAI_MODEL_SUGGESTIONS`, the six
`/messages` models go into `ANTHROPIC_MODEL_SUGGESTIONS`, and neither
model id appears in both.

## OpenCode Free — verified in, not out

v1 of this plan (implemented and shipped as v0.3.3) originally left the
no-auth "OpenCode Free" tier (`opencode.js` in the reference) out of scope,
because the reference implementation always sends a static extra header,
`x-opencode-client: desktop`, that `Provider`/`HttpAdapter` has no field
for (`Provider` carries `wire_format`/`base_url`/`api_key`/`upstream_model`
only — no free-form header map, unlike the reference's per-provider
`transport.headers`), and it wasn't clear whether OpenCode's free tier
*enforces* that header or merely uses it for its own analytics.

That's now settled empirically, with real requests against the live
endpoint (no reference-repo guessing):

```
$ curl https://opencode.ai/zen/v1/chat/completions \
    -H "Authorization: Bearer public" -H "Content-Type: application/json" \
    -d '{"model":"deepseek-v4-flash-free","messages":[{"role":"user","content":"reply with exactly: hi"}],"max_tokens":10,"stream":false}'
HTTP 200 — real completion back, no x-opencode-client header sent.
```

Streaming (`stream:true`) returns standard `chat.completion.chunk` SSE,
and `GET https://opencode.ai/zen/v1/models` (also without the header)
lists 61 model ids, both confirming the same `derive_models_url`-based
discovery that already works for OpenCode Go works here unmodified. The
header is therefore not load-bearing — it's a client-identification nicety
the reference implementation's own executor happens to send, not a gate
this endpoint enforces. That means the only piece OpenCode Free actually
needs — a fixed `Authorization: Bearer public` credential the operator
never types — is achievable with **zero** `HttpAdapter`/`Provider` schema
changes: `HttpAdapter` already sends `Authorization: Bearer {api_key}` for
OpenAI wire whenever one is present, so storing the literal string
`"public"` as the provider's `api_key` reproduces the reference's behavior
exactly.

The one real gap is UX, not capability: today's `ProviderTemplate` only
prefills `wire_format`/`base_url`/`upstream_model` — a passthrough
provider's `api_key` is always a blank prompt (CLI `Password` prompt,
frontend password input), because every other template needs a *real*
secret the operator must supply. OpenCode Free is the first template
whose credential is a public, non-secret constant, so it's the first one
worth prefilling:

- **`ProviderTemplate`** (both `src/onboarding.rs` and
  `frontend/src/pages/Providers.tsx`) gains an optional
  `api_key: Option<&'static str>` / `api_key?: string` field, `None`/
  absent for every existing template.
- **CLI wizard**: the `Password` prompt for API key stays exactly as
  written (dialoguer's `Password` widget can't show a visible default —
  that would defeat the point of masking input), but after collecting it,
  an empty/whitespace-only answer falls back to the chosen template's
  `api_key` if one was set. Pressing Enter on an empty prompt is how the
  operator accepts "use the free public key."
- **Admin UI**: choosing the template prefills the (visible, editable)
  API key field with `"public"` directly, the same way base_url and
  upstream_model are already prefilled — no extra interaction beyond
  picking the template and clicking Save.

Preset values for the new template:

| Label | wire_format | base_url | default upstream_model | default api_key |
|---|---|---|---|---|
| OpenCode Free | openai | `https://opencode.ai/zen/v1/chat/completions` | `deepseek-v4-flash-free` | `public` |

`deepseek-v4-flash-free` (one of the eight explicitly `-free`-suffixed
model ids in the catalog) is picked as the default specifically because
it was the one used in the verification request above — the other ~53
model ids in the free catalog (`gpt-5.4`, `claude-sonnet-5`, etc.) are
almost certainly metered/quota'd behind OpenCode's own account system
despite answering to the same `Bearer public` credential in these tests,
and this plan makes no claim about their rate limits or terms of use.
Operators wanting one of those can still type any model id into the
free-text upstream-model field, exactly as with every other passthrough
provider — the template only seeds a starting default known to work
without further configuration.

## Non-goals

- No new `ProviderKind`, no migration, no new adapter module.
- No attempt to keep the 14 hardcoded model ids fresh automatically beyond
  what "Fetch models" already does for any passthrough provider — the
  reference repo's own model list will drift too (see the DeepSeek
  `deepseek-chat` → `deepseek-v4-flash` rename called out in `README.md`
  as the precedent for why these are suggestions, not validated enums).
