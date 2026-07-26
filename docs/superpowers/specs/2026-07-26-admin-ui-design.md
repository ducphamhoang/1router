# 1router Admin UI — Design

> **Revision note:** this spec was reviewed by two independent agents (Opus,
> Codex) after the first draft. Both converged on the same set of critical
> gaps — a non-functional "shared secret reload" claim, a weak default
> password, an unspecified signing-key store, missing CSRF/brute-force
> protection, no session cleanup, and an unwired Node/npm build dependency.
> This revision fixes all of them; see inline notes marked **(review fix)**.

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
new axum route. **(review fix, axum 0.7 route syntax)** axum 0.7/matchit 0.7
requires a named wildcard — `GET /ui/*path`, not `GET /ui/*` — plus an
explicit `GET /ui` → 308 redirect to `/ui/`, matching the `{id}`-vs-`:id`
gotcha CLAUDE.md already warns about. The handler falls back to `index.html`
for any unmatched sub-path so client-side routing (`/ui/providers`, etc.)
survives a hard refresh.

**(review fix, unwired build dependency — I6/#7)** Node is a genuinely new
hard build dependency and neither the release workflow
(`.github/workflows/release.yml`) nor the `Dockerfile` currently have it: the
release workflow's 4 platform legs run `cargo build --release` directly with
no `setup-node` step, and the Dockerfile's builder stage is a bare
`rust:1.90-alpine` installing only `musl-dev`/`sqlite-static`. To avoid
breaking either:

- The embed lives behind a new `ui` Cargo feature, **on by default** for
  normal builds but explicitly disabled (`--no-default-features`) in any
  offline/sandboxed context — this is what keeps `cargo build --offline`/`cargo
  test --offline` working per CLAUDE.md's Codex-sandbox workflow, which has no
  network and can't `npm ci`.
- `build.rs` checks for `frontend/dist/index.html`; if the `ui` feature is on
  and it's missing, `build.rs` shells out to `npm ci && npm run build` itself
  (so a normal `cargo build` "just works" for a dev with Node installed,
  without a separate manual step). If `npm`/`node` aren't on `PATH`, it fails
  fast with a clear error naming the missing binary, rather than a confusing
  `rust-embed` compile error about a missing directory.
- `.github/workflows/release.yml` gets an `actions/setup-node@v4` step (pinned
  LTS version) before `cargo build --release`, once per matrix leg.
- The `Dockerfile` builder stage adds `apk add --no-cache nodejs npm` and
  copies `frontend/` before the `cargo build` line (so Docker's layer cache
  still invalidates correctly on frontend-only changes vs. Rust-only changes —
  order: `COPY frontend/package*.json`, `npm ci`, `COPY frontend/`, `npm run
  build`, then the existing Rust `COPY`/`cargo build` steps).

Single binary, single port — same deployment story as today.
`frontend/node_modules/` and `frontend/dist/` are gitignored — only source is
committed.

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
CREATE INDEX idx_admin_sessions_expires ON admin_sessions(expires_at);

-- (review fix, C4/I3 signing-key store) one-row k/v table for
-- server-managed secrets that aren't the shared secret or a user password.
CREATE TABLE server_secrets (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

**(review fix, C3 — weak default password, both agents' top Critical finding.)**
`admin_users` is seeded on first boot if empty. This is a fully separate
bootstrap flow from the shared secret's — different table, different
credential, never shared or merged — but it mirrors the same **dual
TTY-vs-headless pattern** `main.rs`/`onboarding.rs` already use for the shared
secret (`main.rs:48-66`: `onboarding::resolve_or_prompt_secret` when a TTY is
present, `config::generate_secret()` + `persist_secret()` + log-once when
headless), because it's the same underlying problem — a credential must exist
before first use, and a human at a terminal should get to choose one they'll
actually remember rather than being handed a random string to memorize:

- **Interactive (TTY present, e.g. running `1router setup`)**: prompt the
  operator to type their own admin password directly (same UX shape as the
  existing shared-secret prompt), confirm it, hash with argon2, store in
  `admin_users`. Username is always `admin` (not a secret, no prompt needed).
- **Headless (no TTY, e.g. plain Docker boot)**: no one to prompt, so
  auto-generate a random password, hash it, store it, and log the plaintext
  **exactly once** at startup (`tracing::info!`, same level/treatment as the
  shared secret today) — the expectation here matches the shared secret's
  existing headless path: whoever's provisioning the deployment captures it
  into their secrets manager at deploy time.
- Either path: no forced rotation on first login (matches the earlier
  decision to keep this a simple single-operator tool). `PATCH
  /admin/auth/password` (below) lets the operator set something memorable at
  any time after — the immediate next step for anyone who got the
  auto-generated headless value and wants their own.

**New `admin::auth` module**:

- `POST /admin/auth/login` — verify `username`/`password` against
  `admin_users` (argon2 verify, which is constant-time by construction — no
  additional care needed there), on success generate a random 256-bit session
  token, store its SHA-256 hash + expiry in `admin_sessions`, set it as an
  HTTP-only, `Secure`, `SameSite=Strict` cookie (`__Host-` prefix when served
  over TLS). **(review fix, I3 — brute-force)** failed attempts are tracked
  per-source-IP in the existing `AppState`'s `DashMap`-based runtime state
  (same pattern already used for provider backoff state,
  `src/proxy/backoff.rs`): exponential backoff after 5 failures in a rolling
  window, capped at a 5-minute lockout, reset on success. Login failures are
  logged (username attempted, source IP, timestamp) but never the password.
- `POST /admin/auth/logout` — deletes **all** session rows for the account
  (not just the current one — review fix, I4b), clears the cookie.
- `PATCH /admin/auth/password` — change password (requires a valid session),
  re-hashes, updates `admin_users`, and deletes all other existing sessions
  except the one making this request (a changed password should invalidate
  stolen/leaked sessions, same reasoning as forcing logout everywhere else).

**(review fix, I2 — login endpoint would otherwise gate itself.)** The
`/admin/*` router splits into two axum layers instead of the single `guarded`
merge in `src/app.rs`: an unauthenticated stratum containing only `POST
/admin/auth/login` (and the static `/ui/*` asset handler, which serves the SPA
shell itself — the SPA's own JS is what redirects to a login screen on a 401
from the API, not the server), and everything else under
`require_admin_session`.

**New `require_admin_session` middleware** replaces `require_bearer` on the
authenticated `/admin/*` stratum. **(review fix, I1 — non-breaking for
existing curl/script users.)** Rather than a hard swap, it accepts *either* a
valid session cookie *or* the existing shared-secret `Bearer` header — whichever
is present and valid. This keeps `/admin/export`/`/admin/import` usable
head-lessly via curl/CI exactly as the Non-goals section already promises, and
means nothing that scripts against `/admin/*` today breaks. It reads the
session cookie, hashes it (SHA-256, no separate signing needed — see below),
looks up `admin_sessions`, checks expiry, and renews expiry only when **less
than 50% of the TTL remains** (review fix, Minor — avoids a DB write on every
single admin request) up to an **absolute session lifetime cap** (e.g. 7 days
from creation, regardless of renewal — a stolen cookie cannot renew forever).
`/v1/*` (the proxy router) is completely untouched — still `require_bearer`
against the shared secret only, no session-cookie fallback there.

**(review fix, C4 — signing key was hand-waved, and is actually unnecessary.)**
Both review agents pointed out the same thing: since the session token is a
random 256-bit value looked up by its hash (not decoded/parsed), a separate
HMAC signature adds no real security — forgery is already infeasible without
guessing a 256-bit random value. **The design drops cookie signing entirely.**
This removes the "where does the signing key live" question altogether rather
than half-answering it. (The new `server_secrets` table above is kept anyway —
it's the right place for the session-cleanup task's bookkeeping and any future
server-managed value that _does_ need persisted state, but session auth itself
doesn't depend on it.)

**(review fix, I4a — unbounded `admin_sessions` growth.)** A background sweep
task, following the exact existing precedent of
`providers::refresh_task::spawn_background_refresh`
(`src/providers/refresh_task.rs`): a tokio interval (e.g. every 10 minutes)
runs `DELETE FROM admin_sessions WHERE expires_at < now`. Also run once at
boot (covers a long-downtime restart). The new `idx_admin_sessions_expires`
index above keeps this and the per-request expiry check cheap.

**(review fix, I5 — CSRF, SameSite=Strict alone is necessary but not
sufficient.)** **(implementation-time correction — scope narrowed to
cookie-authenticated requests only.)** All non-`GET` `/admin/*` requests
authenticated via the **session cookie** must carry a custom header (e.g.
`X-Requested-With: 1router-ui`) or be rejected with 403. A cross-site
form/fetch can trigger a same-site-cookie-bearing request but cannot set a
custom header without CORS preflight, and no CORS policy is configured to
allow that — so this closes the gap cheaply without a token-issuance scheme.
`apiClient.ts` sets this header on every mutating request.

This check does **not** apply to requests authenticated via the shared-secret
Bearer header: a CSRF attack works only because a browser automatically
attaches cookies to a cross-site request — an `Authorization: Bearer` header
is never automatically attached that way, so a Bearer-authenticated request
cannot be forged by a CSRF attack in the first place. Requiring the header
there too would have broken this spec's own stated goal (`require_admin_session`
accepts cookie OR Bearer specifically so existing curl/CI usage of `/admin/*`
keeps working unmodified) — an early implementation applied the header check
universally before authentication, which technically satisfied the letter of
this bullet but silently broke every Bearer-authenticated script until
caught by 6 pre-existing integration tests failing during Phase E
integration. The check now lives inside `require_admin_session` itself:
cookie-authenticated → CSRF header required; Bearer-authenticated → exempt;
neither → 401 (CSRF is irrelevant once auth has already failed).
`require_csrf_header` remains applied to the login endpoint specifically
(which has no Bearer path and still needs protection against login-CSRF).

**Shared secret becomes an editable setting.**
**(review fix, C1/C2 — the original spec's "existing snapshot/reload
mechanism" doesn't exist and would silently no-op; also, env-var precedence
would silently undo a persisted change.)** Concretely:

- `AppState` gains `shared_secret: Arc<ArcSwap<String>>` (alongside the
  existing `snapshot: Arc<ArcSwap<ConfigSnapshot>>` — same pattern, new
  field), initialized from `resolve_shared_secret()`'s result at boot exactly
  as today.
- `require_bearer` (`src/auth/middleware.rs`) reads
  `state.shared_secret.load()` instead of `state.config.shared_secret`.
- New `GET/PATCH /admin/settings/shared-secret`: `GET` returns the secret
  **masked** by default (matching the existing `api_key` masking convention on
  `/admin/providers`), with an explicit `?reveal=true` for the real value.
  `PATCH` calls `config::persist_secret()` (already implemented, writes the
  0600 sidecar file) and then `state.shared_secret.store(Arc::new(new_secret))`
  — live, no restart, this time for real.
  - If the active secret's `SecretSource` was `Env` (i.e.
    `ROUTER_SHARED_SECRET` is set), `PATCH` returns `409 Conflict` with a
    message explaining the env var takes precedence on every restart and must
    be changed/unset there instead — rather than silently persisting a sidecar
    value that reverts on next boot.

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
  paste-code input then calls `POST .../oauth/complete`. **(review fix, Opus
  Minor — callback UX gap)** OpenAI's redirect target is a fixed
  `http://localhost:1455/auth/callback` with nothing listening there (see the
  original design spec's OAuth flow section) — the browser will show a
  connection-error page after consent, and the `code` param must be copied out
  of that page's address bar. The panel's copy must say this explicitly up
  front ("after you approve, the page will fail to load — that's expected,
  copy the `code` value from the URL bar"), or first-time users will think the
  flow is broken.
- **Settings page**: change admin password (`PATCH /admin/auth/password`);
  view/edit the shared secret (`GET/PATCH /admin/settings/shared-secret`,
  masked by default with a reveal toggle).

`apiClient.ts` centralizes cookie-based fetches (`credentials: 'include'`),
attaches the `X-Requested-With: 1router-ui` header on every non-`GET` request
(CSRF mitigation, see Backend changes), and redirects to `/ui/login` on any
401, so individual pages don't handle auth failure themselves.

## Testing plan

**Backend (`cargo test --offline`, with the `ui` feature disabled so this
suite has no Node dependency — only the `admin::auth`/session/secret logic is
Rust, unaffected by the frontend build)**:
- Password hashing/verification (argon2 round-trip, wrong-password rejection),
  and that the bootstrap password is randomly generated (not a fixed value)
  each time the seed runs on an empty `admin_users`.
- Session token issuance, cookie validation, expiry, sliding renewal (renews
  only under 50% TTL remaining), and the absolute lifetime cap rejecting an
  old-but-repeatedly-renewed token.
- `require_admin_session` middleware: 401 with neither a valid cookie nor a
  valid Bearer secret; 200 with either one alone — same pattern as the
  existing `require_bearer` tests in `src/app.rs`, extended for the
  cookie-or-Bearer fallback (review fix, I1).
- Login/logout/password-change handlers; login lockout after repeated
  failures and reset-on-success; logout and password-change both invalidate
  other sessions.
- Shared-secret settings GET (masked vs. `?reveal=true`) / PATCH, including
  the `409` case when the active secret came from `ROUTER_SHARED_SECRET`, and
  a test proving `/v1/*` auth actually observes a PATCHed secret without a
  restart (the single riskiest behavior in this spec, and the one the
  original draft got wrong).
- CSRF header enforcement: a cookie-authenticated mutating request missing
  `X-Requested-With` is rejected (403); a Bearer-authenticated mutating
  request missing it is NOT rejected for that reason (Bearer is exempt — see
  above); the login endpoint (no Bearer path) still requires it.
- Session cleanup sweep: expired rows are deleted by the background task and
  by the boot-time sweep.
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
would: a new migration, a new `admin::auth` module, a mutable
`AppState.shared_secret`, a login-rate-limiter, a background session-cleanup
task, and a new settings endpoint. It is additive and isolated — `/v1/*` proxy
logic and the existing passthrough/Codex adapters are untouched, and
`require_admin_session`'s Bearer fallback means no existing curl/CI usage of
`/admin/*` breaks — but it is a real backend feature addition, not just static
assets served from the binary.

## Open questions carried into implementation planning

- Exact argon2 parameters (memory/time cost) — default `argon2` crate params
  are almost certainly fine for a single-operator tool, but worth a one-line
  confirmation in the implementation plan rather than silently picking
  whatever the crate defaults to.
- Whether the login rate-limiter's per-IP state should survive a restart
  (currently proposed as in-memory/`DashMap`, matching `proxy::backoff`'s
  existing pattern — resets on restart, which is an acceptable and consistent
  trade-off, not a gap, but calling it out explicitly).
