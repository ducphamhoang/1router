# 1router

A lean Rust rewrite of an LLM API gateway: config-only OpenAI/Anthropic-compatible
passthrough providers, plus a Codex OAuth adapter (ChatGPT-subscription auth),
fronted by a single admin-secret-protected HTTP API.

## Quickstart (interactive)

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
