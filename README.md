# 1router

A lean Rust rewrite of an LLM API gateway: config-only OpenAI/Anthropic-compatible
passthrough providers, plus a Codex OAuth adapter (ChatGPT-subscription auth),
fronted by a single admin-secret-protected HTTP API.

## Getting started

### Option A — build from source

```
cargo build --release
./target/release/1router setup      # interactive first-time setup
./target/release/1router            # start the server
```

### Option B — Docker

```
docker build -t 1router .
mkdir -p data
docker run -it --rm -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db 1router setup
docker run -d --name 1router -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db 1router
```
(`-it` on the first `run` is required — `setup` is interactive and needs a
real terminal attached; the second, normal-boot `run` doesn't need it.)

### Interactive setup wizard

    1router setup

Walks you through: creating an admin secret (stored in `.router_secret` next to
the SQLite file, mode 0600), adding one provider — either a passthrough
OpenAI/Anthropic-compatible endpoint, or a Codex/ChatGPT account via OAuth (the
wizard probes which `upstream_model` your account accepts) — and putting it in
a pool. It then offers to add that same provider to further pools under a
different `model_override` (e.g. one Codex OAuth login backing separate
`codex-sol`/`codex-terra`/`codex-luna` pools) without repeating the OAuth
dance or creating a duplicate provider row. Then just run `1router`.

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
clients put in `model`. There's no way to address a provider directly; every
call routes through a pool, even a 1-member one. The setup wizard and the
admin UI's "Make it directly callable" checkbox (see "Admin web UI" below)
both default to creating that 1-member pool automatically, using the
provider's own id/name, so in practice a single-provider setup still reads
as "call the model by name" — e.g. a provider named `deepseek-flash` becomes
`"model":"deepseek-flash"` with no extra pool-management step. Reach for a
multi-member pool only when you want round-robin or failover across more
than one provider under one name.

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
       pool lookup (by `model`)          SQLite: providers, pools,
           │                              pool_members, provider_oauth_state
           ▼
   priority-ordered providers  ──fail over on retryable errors──▶ next provider
           │
           ▼
     ProviderAdapter (per provider `kind`)
       ├─ passthrough:  forwards the request as-is (OpenAI or Anthropic wire
       │                format) to `base_url`, `Authorization`/`x-api-key` set
       │                from the stored `api_key`.
       └─ oauth_codex:  rewrites Chat-Completions `messages` into the
                        Responses API's `input`, forces `store`/`stream`,
                        strips fields Codex's backend rejects, and targets
                        chatgpt.com/backend-api/codex/responses using a
                        refreshable OAuth access token instead of a static key.
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
existing pool yourself instead. A preset dropdown (OpenAI/Anthropic/DeepSeek)
pre-fills wire format, base URL, and model — every field stays editable
after picking one.

**Pools page**: pools are listed by name; tapping one opens a dialog to
manage its providers — reorder (drag or ↑/↓), remove, or add a provider with
an optional per-pool `model_override` (what lets one provider's credentials
serve several pools calling different upstream models). The model field
offers common GPT/Claude model names as suggestions but takes any free text,
and a **Validate** button next to it sends a real one-token "hi" request
through that provider's own adapter to confirm the model name is actually
callable before you save it.

## Admin API

Everything the web UI does is also available as a plain HTTP API
(`POST /admin/providers`, `POST /admin/pools`, `PUT /admin/pools/:id/members`,
`POST /admin/providers/:id/validate-model`, etc.), behind the same
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
