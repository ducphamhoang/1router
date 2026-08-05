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

## Out of scope (v1): OpenCode Free

The no-auth "OpenCode Free" tier (`opencode.js` in the reference) needs two
things the config-only preset model can't express:

1. A **fixed** `Authorization: Bearer public` credential the operator never
   types — mechanically achievable by literally storing the string
   `"public"` as the provider's `api_key`, since `HttpAdapter` already
   sends `Authorization: Bearer {api_key}` for OpenAI wire whenever one is
   present.
2. A **static extra header**, `x-opencode-client: desktop`, that
   `Provider`/`HttpAdapter` has no field for today (`Provider` carries
   `wire_format`/`base_url`/`api_key`/`upstream_model` only — no
   free-form header map, unlike the reference's per-provider
   `transport.headers`).

(1) is free; (2) is a real gap, and it's not clear from the reference
alone whether OpenCode's free tier actually *enforces* that header or
merely uses it for its own analytics — guessing wrong either breaks the
free tier silently or ships an unnecessary adapter change. Given the paid
"OpenCode Go" templates above already cover the primary use case (a
5/mo account with real model variety) with **zero** adapter risk, OpenCode
Free is left as a follow-up: either confirm the header is unnecessary (and
ship it as a third preset with `api_key: "public"`), or add a generic
`extra_headers: Option<serde_json::Value>` column to `Provider` and thread
it through `HttpAdapter::build_request` — the latter is a real schema
change and shouldn't ride on this otherwise code-free plan.

## Non-goals

- No new `ProviderKind`, no migration, no new adapter module.
- No attempt to keep the 14 hardcoded model ids fresh automatically beyond
  what "Fetch models" already does for any passthrough provider — the
  reference repo's own model list will drift too (see the DeepSeek
  `deepseek-chat` → `deepseek-v4-flash` rename called out in `README.md`
  as the precedent for why these are suggestions, not validated enums).
