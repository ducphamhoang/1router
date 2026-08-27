# Architecture & reference

This is the deep-dive companion to the [README](../README.md) — wire-format
translation rules, the full environment-variable list, the admin API, and
the request path end to end. Read this if you're extending 1router,
scripting against its admin API, or just curious how it works. If you just
want to run it, the README is enough.

## Wire formats

Two client-facing routes: `POST /v1/chat/completions` (OpenAI Chat
Completions shape) and `POST /v1/messages` (Anthropic Messages shape, what
Claude Code speaks). Every provider kind can serve either route: a
provider's own `wire_format` describes what its *upstream* speaks, not what
clients are limited to. If a client's request shape doesn't match the
provider's own format, 1router translates it (and translates the response
back) — a plain passthrough Anthropic-compatible endpoint (e.g. an
`api.anthropic.com`-style provider) can serve `/v1/chat/completions`
clients directly, and vice versa, with no per-provider config needed beyond
its own `wire_format`. This holds for streaming responses too. The
Codex/ChatGPT-OAuth and Command Code adapters translate the same way, just
against their own proprietary upstream shapes instead of a second wire
format.

One thing this does *not* change: a **pool**'s own `wire_format` still pins
it to one client-facing route, set at creation - a pool created with
`wire_format: anthropic` only ever answers `/v1/messages`, regardless of
what any of its member providers' own formats are. Serving both client
routes from one credential still means either direct `<provider_id>/<model>`
addressing (works from either route now, for every provider kind) or two
separate pool rows for the same provider - e.g. one Codex OAuth login
backing an `openai`-format pool for OpenCode and an `anthropic`-format pool
for Claude Code simultaneously, no duplicate provider/OAuth needed.

## Addressing: pools vs. direct `<provider_id>/<model>`

`<pool-id>` is whatever you name the pool on the Pools page — that's the value
clients put in `model` for round-robin/failover across one or more
providers under a shared name. Pools are created explicitly now: the setup
wizard and the admin UI no longer auto-create a matching pool when a
provider is added, because direct addressing (below) already makes every
provider's models callable by name with no extra step.

For a provider that offers several models, you don't need a 1-member pool
per model just to make each one callable - `model` also accepts
`<provider_id>/<model-name>` (e.g. `"model":"deepseek_api/deepseek-v4-pro"`),
which routes directly to that one provider with that exact model, no pool
involved. This is a pure fallback: it only kicks in when `model` doesn't
match a real pool id, and since pool/provider ids can never contain `/`
(rejected at creation), the two addressing modes can never collide. The
tradeoff versus a real pool: no round-robin/failover, since you named one
specific provider - that's the whole point of using it over a pool.

`GET /v1/models` lists these `<provider_id>/<model>` combinations too,
alongside pool ids, for every passthrough or Command Code provider whose own
models endpoint has been fetched. That fetch happens automatically and in
the background - right after a provider is created, and once for every
existing provider at boot (so upgrading to this feature doesn't require
manually re-adding providers) - and is cached in memory, so `/v1/models`
itself never makes a network call. The cache is lost on restart (re-warmed
at the next boot) and a provider whose fetch fails (dead upstream, no
`/models` support) is simply absent from the list rather than causing an
error.

## Dataset logging

Opt-in, off by default: a provider or a specific pool membership can be
flagged to capture the raw request/response bytes of every successful
exchange it serves, as JSONL files on disk — a corpus usable later for
fine-tuning/distillation curation. Two-layer toggle, same
nullable-falls-back idiom as `model_override`:

- `Provider.dataset_logging` (admin UI: the "Log requests/responses for
  this provider" checkbox on the Providers page) is the base setting —
  also the only one consulted for `<provider_id>/<model>` direct
  addressing, which has no pool membership to override it.
- `PoolMember.dataset_logging_override`, set per-membership on the Pools
  page, overrides the provider's setting for that specific pool only —
  lets one credential shared across several pools log some and not others.

Records land at `<dataset_log_dir>/{provider_id}/{YYYY-MM-DD}.jsonl`
(`dataset_log_dir` defaults to a `dataset-logs` folder next to the sqlite
file, override with `ROUTER_DATASET_LOG_DIR`), one JSON line per
successful exchange: raw client-facing request/response bytes exactly as
sent (no parsing, no cross-wire normalization — that's left to whatever
offline curation step consumes the corpus), `complete: false` when a
response ended early (client disconnect or an upstream error mid-stream,
still logged with whatever was captured so far), and no record at all for
failed/errored exchanges. Bodies are captured raw and unredacted — this is
deliberate, since enabling the toggle at all is the admin asserting they're
fine capturing what actually flows through that provider/membership;
there's no PII scrubbing pass to get subtly wrong. Full design rationale:
[`docs/superpowers/specs/2026-08-27-dataset-logging-design.md`](superpowers/specs/2026-08-27-dataset-logging-design.md).

## Configuration (environment variables)

| Variable | Default | Purpose |
| --- | --- | --- |
| `ROUTER_LISTEN_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `ROUTER_SQLITE_PATH` | `1router.db` | SQLite database file |
| `ROUTER_SHARED_SECRET` | (none) | Admin secret, used as `Authorization: Bearer <secret>` on `/v1/*` and `/admin/*` |
| `ROUTER_SEED_PATH` | (none) | Path to a JSON config file applied on first boot (providers/pools/members) |
| `ROUTER_MAX_BODY_BYTES` | `10485760` (10 MiB) | Max request body size |
| `ROUTER_CONNECT_TIMEOUT` | `10` (seconds) | Upstream connect timeout |
| `ROUTER_TTFB_TIMEOUT` | `60` (seconds) | Upstream time-to-first-byte timeout |
| `ROUTER_IDLE_TIMEOUT` | `120` (seconds) | Upstream idle timeout |
| `ROUTER_DRAIN_TIMEOUT` | `30` (seconds) | Graceful shutdown drain window |
| `ROUTER_DATASET_LOG_DIR` | `<sqlite file's directory>/dataset-logs` | Where opt-in dataset-logging JSONL files are written — see [Dataset logging](#dataset-logging) |

## Request path end to end

```
client → /v1/chat/completions or /v1/messages
           │
           ▼
       pool lookup (by `model`),         SQLite: providers, pools,
       else <provider_id>/<model>         pool_members, provider_oauth_state
           │
           ▼
   priority-ordered providers  ──fail over on retryable errors──▶ next provider
           │
           ▼
     ProviderAdapter (per provider `kind`)
       ├─ passthrough:  forwards to `base_url` in the provider's own wire
       │                format, translating from the client's if it
       │                differs; `Authorization`/`x-api-key` set from the
       │                stored `api_key`.
       ├─ oauth_codex:  rewrites Chat-Completions `messages` into the
       │                Responses API's `input`, and targets Codex using
       │                a refreshable OAuth access token.
       └─ oauth_command_code: wraps requests for Command Code's proprietary
                              envelope and converts NDJSON into OpenAI or
                              Anthropic streaming responses using its stored key.
           │
           ▼
   upstream provider → response streamed/aggregated back to the client
```

Everything is one SQLite file, one binary, no external services. Admin
state (providers/pools/members, OAuth tokens) lives in that same DB; the
shared admin secret is the only thing that can live outside it (env var or
the `.router_secret` sidecar).

For the full design rationale — DB schema, the `ProviderAdapter` trait,
failover/backoff rules, the Codex OAuth token-refresh/locking scheme, and
what's explicitly out of scope for v1 — see:

- [`superpowers/specs/2026-07-25-1router-design.md`](superpowers/specs/2026-07-25-1router-design.md) — core system design
- [`superpowers/specs/2026-07-26-onboarding-wizard-design.md`](superpowers/specs/2026-07-26-onboarding-wizard-design.md) — the `setup` wizard's design
- [`superpowers/specs/2026-08-04-commandcode-provider-design.md`](superpowers/specs/2026-08-04-commandcode-provider-design.md) — Command Code adapter design
- [`superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`](superpowers/specs/2026-08-04-universal-passthrough-translation-design.md) — universal passthrough wire-format translation design
- [`superpowers/specs/2026-08-05-opencode-preset-provider-design.md`](superpowers/specs/2026-08-05-opencode-preset-provider-design.md) — OpenCode preset provider design
- [`superpowers/plans/`](superpowers/plans/) — task-by-task implementation plans, useful for "why is this code shaped this way" archaeology

## Admin API

Everything the web UI does is also available as a plain HTTP API
(`POST /admin/providers`, `POST /admin/pools`, `PUT /admin/pools/:id/members`,
`POST /admin/providers/:id/validate-model`,
`GET /admin/providers/:id/list-models`, etc.), behind the same
`Authorization: Bearer <admin-secret>` header — useful for scripting or CI.
The setup wizard only adds; use the UI or this API for edits/deletes.

If you script the session-cookie flow instead (calling `POST
/admin/auth/login` yourself rather than passing `Authorization: Bearer
<admin-secret>` on every request), every non-GET `/admin/*` request —
including the login call itself — must also carry `X-Requested-With:
1router-ui`, or it's rejected with `403 {"error": {"message": "missing
X-Requested-With header"}}`. This is the admin UI's CSRF guard; it only
applies to cookie-authenticated requests, so it's a non-issue for the
Bearer-header scripting path above. See the README's
[Admin dashboard](../README.md#admin-dashboard) section for a worked
example.

## Building and testing

```
cargo build --offline
cargo test --offline
```

The `ui` feature (default-on) builds the embedded admin dashboard via `npm`
as part of `cargo build`, so it needs Node.js on the build machine (not at
runtime — the built assets are embedded in the binary). Build with
`--no-default-features` to skip the dashboard entirely and drop that
dependency.

See `CLAUDE.md` for full build/test conventions.
