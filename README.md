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
a pool. Then just run `1router`.

The same wizard runs automatically on first boot when the database is empty,
`ROUTER_SEED_PATH` is unset, and stdin is a terminal.

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
clients put in `model`; there's no way to address a provider directly.

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

## Admin API

Once running, providers/pools/members can be managed via the admin API
(`POST /admin/providers`, `POST /admin/pools`, `PUT /admin/pools/:id/members`,
etc.), all behind the same `Authorization: Bearer <admin-secret>` header. The
wizard only adds — there is no interactive edit/delete flow; use the admin API
for that.

## Build & test

```
cargo build --offline
cargo test --offline
```

See `CLAUDE.md` for full build/test conventions.
