# 1router Admin UI — Design

## Summary

1router's design deliberately excludes a built-in web dashboard (see
`docs/superpowers/specs/2026-07-25-1router-design.md` line 342) — `/admin/*` is
API-only, config happens via `curl`/the `1router setup` CLI wizard. This spec adds
a small embedded single-page admin UI on top of the existing `/admin/*` API,
covering the operations that are genuinely painful to do by hand: editing
providers/pools, reordering pool member priority, running connectivity tests,
and walking through the Codex OAuth flow.

This is additive: `/v1/*` proxy behavior, the passthrough/Codex adapters, and
the existing `/admin/*` request/response shapes are untouched. What changes is
*how you authenticate to* `/admin/*`, plus new endpoints for UI login and
settings.

## Non-goals

- Multi-user accounts, roles, or permissions (single admin account only).
- Export/import JSON UI (deferred — `/admin/export` and `/admin/import` already
  exist as API endpoints and remain CLI/curl-only for now).
- Any change to `/v1/*` proxy auth or request handling.
- Mobile-responsive design, i18n, or theming — this is an internal ops tool.

## Architecture

New `frontend/` directory at the repo root: React + Vite + TypeScript, built via
`npm run build` into `frontend/dist/`. That output is embedded into the
`1router` binary at compile time via `rust-embed` (new dependency), served by a
new axum route (e.g. `GET /ui/*`, falling back to `index.html` for
client-side routing so deep links like `/ui/providers` work on refresh).

Single binary, single port — same deployment story as today. The build/release
pipeline (already a 4-platform binaries matrix, see
`docs/superpowers/specs/2026-07-26-release-publishing-design.md`) gains a
Node/npm build step before each `cargo build`; local dev needs Node/npm
installed too. `frontend/node_modules/` and `frontend/dist/` are gitignored —
only source is committed.

## Backend changes

**New tables** (migration `migrations/0002_admin_ui.sql`):

```sql
CREATE TABLE admin_users (
    id INTEGER PRIMARY KEY CHECK (id = 1),  -- single-row table
    username TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE admin_sessions (
    token_hash TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
```

`admin_users` is seeded on first boot if empty: username `admin`, password
`123456` (argon2-hashed). No forced password change — the user may leave the
default in place.

**New `admin::auth` module**:

- `POST /admin/auth/login` — verify `username`/`password` against
  `admin_users` (argon2 verify), on success generate a random session token,
  store its hash + expiry in `admin_sessions`, set it as a signed, HTTP-only,
  `SameSite=Strict` cookie.
- `POST /admin/auth/logout` — delete the session row, clear the cookie.
- `PATCH /admin/auth/password` — change password (requires a valid session),
  re-hashes and updates `admin_users`.

**New `require_admin_session` middleware** replaces `require_bearer` on the
`/admin/*` router only, in `src/app.rs`'s `build_router`. It reads the session
cookie, hashes it, looks up `admin_sessions`, checks expiry, renews expiry on
successful use (sliding session). `/v1/*` (the proxy router) keeps
`require_bearer` against the shared secret exactly as today — the two auth
mechanisms are fully independent.

**Shared secret becomes an editable setting**: new `GET/PATCH
/admin/settings/shared-secret`, gated by `require_admin_session` like the rest
of `/admin/*`. Changing it updates the value read by `/v1/*`'s `require_bearer`
(via the existing config snapshot/reload mechanism — no restart required).

**Session signing**: a random signing key is generated on first boot (stored
alongside other server-managed state, distinct from both the shared secret and
user passwords) and used to sign the session cookie so it can't be forged
without server-side compromise.

## Frontend structure

```
frontend/
  src/
    pages/
      Login.tsx
      Providers.tsx      -- table + create/edit modal + Test button + state badge
      Pools.tsx           -- list + create/delete + drag-to-reorder members (dnd-kit)
      Settings.tsx        -- change admin password, view/edit shared secret
    components/
      CodexOAuthPanel.tsx -- embedded in provider edit modal when kind == oauth_codex
    lib/
      apiClient.ts        -- fetch wrapper, credentials: 'include', 401 -> redirect to /ui/login
```

- **Providers page**: list (name, wire_format, kind, upstream_model, masked
  api_key, live state badge polled every ~5s from `GET
  /admin/providers/:id/state`), create/edit modal, delete, Test button wired to
  `POST /admin/providers/:id/test`.
- **Pools page**: create/delete pools; member list reordered via drag-and-drop
  (`dnd-kit`), persisted via `PUT /admin/pools/:id/members` with the new
  priority order.
- **Codex OAuth panel**: shown inside the provider edit modal for
  `kind: oauth_codex`. Two-step guided flow: "Start" button calls `POST
  .../oauth/start`, opens the returned `authorize_url` in a new tab; a
  paste-code input then calls `POST .../oauth/complete`.
- **Settings page**: change admin password (`PATCH /admin/auth/password`);
  view/edit the shared secret (`GET/PATCH /admin/settings/shared-secret`).

`apiClient.ts` centralizes cookie-based fetches and redirects to `/ui/login` on
any 401, so individual pages don't handle auth failure themselves.

## Testing plan

**Backend (`cargo test --offline`)**:
- Password hashing/verification (argon2 round-trip, wrong-password rejection).
- Session token issuance, cookie validation, expiry, sliding renewal.
- `require_admin_session` middleware: 401 without a valid cookie, 200 with one
  — same pattern as the existing `require_bearer` tests in `src/app.rs`.
- Login/logout/password-change handlers; shared-secret settings GET/PATCH.
- Integration test via `tests/common::spawn_app`: login -> cookie -> an
  authenticated `/admin/providers` call, end to end. Per CLAUDE.md, tests using
  `spawn_app` bind a real socket and are BLOCKED in the Codex sandbox — run
  these manually outside the sandbox to verify.

**Frontend (Vitest + React Testing Library)**:
- `apiClient` 401-redirect behavior.
- Pool member reorder logic (priority recomputation on drag).
- Form validation (provider create/edit, login, password change).
- No e2e/browser test suite planned for v1; manual smoke test via the `run`
  skill once the UI is built and wired up.

## Scope note

This touches the Rust codebase more than a purely cosmetic frontend bolt-on
would: a new migration, a new `admin::auth` module, a middleware swap on the
`/admin/*` router, and a new settings endpoint. It is additive and isolated —
`/v1/*` proxy logic and the existing passthrough/Codex adapters are untouched
— but it is a real backend feature addition, not just static assets served
from the binary.
