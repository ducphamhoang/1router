# 1router

A lean Rust rewrite of an LLM API gateway: config-only OpenAI/Anthropic-compatible
passthrough providers, plus bespoke Codex OAuth and Command Code adapters,
fronted by a single admin-secret-protected HTTP API.

## Getting started

### Option A — download a prebuilt binary

Every tagged release publishes binaries for Linux (x86_64/arm64) and macOS
(Intel/Apple Silicon) on the
[Releases page](https://github.com/ducphamhoang/1router/releases/latest), as
`.tar.gz` archives plus a `SHA256SUMS` file to verify them against.

```
curl -LO https://github.com/ducphamhoang/1router/releases/latest/download/1router-<version>-<target>.tar.gz
tar -xzf 1router-<version>-<target>.tar.gz
./1router setup      # interactive first-time setup
./1router             # start the server
```

`<target>` is one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin` — pick the one matching your OS
and CPU architecture.

### Option B — Docker

A multi-arch image (`linux/amd64`, `linux/arm64`) is published alongside each
release to GHCR:

```
mkdir -p data
docker run -it --rm -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db ghcr.io/ducphamhoang/1router:latest setup
docker run -d --name 1router -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db ghcr.io/ducphamhoang/1router:latest
```
(`-it` on the first `run` is required — `setup` is interactive and needs a
real terminal attached; the second, normal-boot `run` doesn't need it. Pin to
a specific release instead of `:latest` with `ghcr.io/ducphamhoang/1router:vX.Y.Z`.)

To build the image yourself instead of pulling it, replace the image
reference with `1router` after running `docker build -t 1router .`.

### Option C — build from source

```
cargo build --release
./target/release/1router setup      # interactive first-time setup
./target/release/1router            # start the server
```

### Interactive setup wizard

    1router setup

Walks you through: creating an admin secret (stored in `.router_secret` next to
the SQLite file, mode 0600), adding one provider — either a passthrough
OpenAI/Anthropic-compatible endpoint, a Codex/ChatGPT account via OAuth (the
wizard probes which `upstream_model` your account accepts), or a Command Code
account via browser login with a paste-key fallback — and putting it in
a pool. It then offers to add that same provider to further pools under a
different `model_override` (e.g. one Codex OAuth login backing separate
`codex-sol`/`codex-terra`/`codex-luna` pools) without repeating the OAuth
dance or creating a duplicate provider row. A Codex provider can also be
added to pools of *either* client-facing wire format at once - the same
ChatGPT account can back an `openai`-format pool for OpenAI-SDK clients
(Cursor, OpenCode, ...) and an `anthropic`-format pool for Claude Code
simultaneously, with per-request translation handling the difference (see
"Wire formats" below). Then just run `1router`.

The same wizard runs automatically on first boot when the database is empty,
`ROUTER_SEED_PATH` is unset, and stdin is a terminal.

### Forgot the admin UI password?

    1router setup --reset-admin-password

Prompts for a new admin UI password (username stays `admin`) and logs out
every existing session. No current-password check: anyone who can run the
CLI already has filesystem access to the sqlite DB and `.router_secret`, so
requiring the old password here wouldn't add real protection — it would just
remove the only recovery path for an operator who forgot it.

**Headless deployments** (Docker, systemd) never prompt: set
`ROUTER_SHARED_SECRET` (recommended) and `ROUTER_SEED_PATH` to a config JSON
file. If no secret is available at all, one is generated, saved to
`.router_secret`, and logged **once** at startup.

Secret resolution order: `ROUTER_SHARED_SECRET` → `.router_secret` sidecar →
generate one. A sidecar file that exists but can't be read is a fatal startup
error, never silently replaced.

### First request

Once a provider is set up and the server is running:

```
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $(cat .router_secret)" \
  -H 'Content-Type: application/json' \
  -d '{"model":"<pool-id>","messages":[{"role":"user","content":"hi"}]}'
```

`<pool-id>` is whatever you named the pool during setup — that's the value
clients put in `model` for round-robin/failover across one or more
providers under a shared name. The setup wizard and the admin UI's "Make it
directly callable" checkbox (see "Admin web UI" below) both default to
creating a matching 1-member pool automatically, so a single-provider setup
reads as "call the model by name" with no extra step.

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
models endpoint has
been fetched. That fetch happens automatically and in the background -
right after a provider is created, and once for every existing provider at
boot (so upgrading to this feature doesn't require manually re-adding
providers) - and is cached in memory, so `/v1/models` itself never makes a
network call. The cache is lost on restart (re-warmed at the next boot) and
a provider whose fetch fails (dead upstream, no `/models` support) is
simply absent from the list rather than causing an error.

### Wire formats

Two client-facing routes: `POST /v1/chat/completions` (OpenAI Chat
Completions shape) and `POST /v1/messages` (Anthropic Messages shape, what
Claude Code speaks). Every provider kind can serve either route: a
provider's own `wire_format` describes what its *upstream* speaks, not what
clients are limited to. If a client's request shape doesn't match the
provider's own format, 1router translates it (and translates the response
back) - a plain passthrough Anthropic-compatible endpoint (e.g. an
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

## Architecture at a glance

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

Everything is one SQLite file, one binary, no external services. Admin state
(providers/pools/members, OAuth tokens) lives in that same DB; the shared
admin secret is the only thing that can live outside it (env var or the
`.router_secret` sidecar).

For the full design rationale — DB schema, the `ProviderAdapter` trait,
failover/backoff rules, the Codex OAuth token-refresh/locking scheme, and
what's explicitly out of scope for v1 — see:

- [`docs/superpowers/specs/2026-07-25-1router-design.md`](docs/superpowers/specs/2026-07-25-1router-design.md) — core system design
- [`docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md`](docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md) — the `setup` wizard's design
- [`docs/superpowers/specs/2026-08-04-commandcode-provider-design.md`](docs/superpowers/specs/2026-08-04-commandcode-provider-design.md) — Command Code adapter design
- [`docs/superpowers/plans/2026-08-04-commandcode-provider-implementation.md`](docs/superpowers/plans/2026-08-04-commandcode-provider-implementation.md) — Command Code implementation plan
- [`docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md`](docs/superpowers/specs/2026-08-04-universal-passthrough-translation-design.md) — universal passthrough wire-format translation (v0.3.2) design
- [`docs/superpowers/plans/2026-08-04-universal-passthrough-translation-implementation.md`](docs/superpowers/plans/2026-08-04-universal-passthrough-translation-implementation.md) — its implementation plan
- [`docs/superpowers/plans/`](docs/superpowers/plans/) — task-by-task implementation plans, useful for "why is this code shaped this way" archaeology

## Admin web UI

With the default `ui` feature enabled, `http://<host>:8080/ui/` serves a small
admin dashboard — Providers, Pools, and Settings — for everything the wizard's
one-shot flow doesn't cover (editing, deleting, reordering).

Login is username `admin` with its own password, separate from
`ROUTER_SHARED_SECRET`: if headless setup didn't seed one (see "Forgot the
admin UI password?" above), one is generated and logged **once** at first
boot the same way the shared secret is. It's session-cookie based, not the
`Authorization: Bearer` header used everywhere else.

**Providers page**: the "New provider" form has a "Make it directly
callable" checkbox, checked by default, which — on save — also creates a
pool with the same id and adds the provider as its only member, so the
provider's id becomes usable as `model` immediately with no separate trip to
the Pools page. Uncheck it if you're adding the provider to fold into an
existing pool yourself instead. A preset dropdown
(OpenAI/Anthropic/DeepSeek/OpenCode) pre-fills wire format, base URL, and
model — every field stays editable after picking one.

**Pools page**: pools are listed by name; tapping one opens a dialog to
manage its providers — reorder (drag or ↑/↓), remove, or add a provider with
an optional per-pool `model_override` (what lets one provider's credentials
serve several pools calling different upstream models). The model field
offers common GPT/Claude/DeepSeek model names as static suggestions, plus a
**Fetch models** button that calls the provider's own `GET .../models` live
and replaces the suggestions with its real, current list — since a
hardcoded list inevitably goes stale (e.g. DeepSeek's `deepseek-chat` ->
`deepseek-v4-flash` rename). This only works for passthrough providers with
a `/models` endpoint; Codex OAuth providers (no discoverable models
endpoint) and mirrors that don't implement `/models` fall back to the
static suggestions with an inline reason why. A **Validate** button next to
it sends a real one-token "hi" request through that provider's own adapter
to confirm the chosen model name is actually callable before you save it.

For a Command Code provider specifically, the Providers page shows a
**Login with browser** button alongside a paste-key fallback (mirroring the
`1router setup` wizard's own Command Code step): it opens commandcode.ai in
a new tab against a short-lived local callback listener the same way the
CLI wizard does, polls for completion, and on success fetches Command
Code's model list and validates the key against it automatically — no
separate "Validate" click needed. This only works when the admin UI is
being viewed on the same machine running 1router, since commandcode.ai's
login page posts the result to `http://localhost:<port>` from the
browser's own machine; a remote admin UI should use the paste-key fallback
instead. The paste-key path validates the key by probing Command Code's own
models with a real minimal request (biased toward cheap/free-tier models
first, so a flagship-only probe doesn't spuriously 403 with
`MODEL_NOT_IN_PLAN` on an otherwise-valid key) rather than trusting the key
at face value. Either path is reflected in `GET /admin/providers`'s
`credential_configured` field, which the UI uses to show "a key is already
saved" instead of implying no credential exists yet.

**Settings page**: the "Connect a client" section shows the base URL, the
shared secret (same reveal/mask flow as elsewhere), and the available
pools grouped by wire format, plus a copy-pasteable curl example - so
wiring up an OpenAI/Anthropic-compatible client is a matter of reading one
section instead of piecing those values together from other pages. Its
"Check providers for available models" button calls every passthrough
provider's live `/models` list and shows what each one actually offers -
useful for spotting a model your provider supports that isn't callable yet
(no pool/`model_override` points at it), so you know what to add on the
Pools page. This is deliberately kept separate from `GET /v1/models`: that
endpoint only ever lists pool ids, i.e. exactly what a client can call
right now - a provider's raw catalog isn't callable until it's actually in
a pool, so mixing the two would make `/v1/models` list non-functional
entries.

## Admin API

Everything the web UI does is also available as a plain HTTP API
(`POST /admin/providers`, `POST /admin/pools`, `PUT /admin/pools/:id/members`,
`POST /admin/providers/:id/validate-model`,
`GET /admin/providers/:id/list-models`, etc.), behind the same
`Authorization: Bearer <admin-secret>` header — useful for scripting or CI.
The setup wizard only adds; use the UI or this API for edits/deletes.

## Build & test

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
