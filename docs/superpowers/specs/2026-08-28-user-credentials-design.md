# User credentials — Design

## Goal

Let an admin create named "users" (really: labeled credentials, not human
accounts with passwords/sessions), each with its own bearer key that can call
`/v1/*` alongside the existing shared secret. Every request authenticated by
a user key gets that user's id attached to `request_log` and (when dataset
logging is on for the resolved provider) `dataset_log` — filling in the
`user_id` field `dataset_log.rs` has reserved as `None` since
`2026-08-27-dataset-logging-design.md`. Purely additive: nothing about the
existing shared-secret path changes.

Scope, per explicit decision (not re-litigated here):

- **Coexists with the shared secret**, doesn't replace it. A request
  authenticated by the shared secret still has `user_id: None`, exactly like
  today — the shared secret is not "a user."
- **No session/login-event concept.** "Log in/out based on user id" means
  *attribute requests to a user in the logs that already exist*
  (`request_log`, `dataset_log`), not a login/logout audit trail or a
  session lifecycle. There is no `/v1/login` endpoint and no per-user
  session table.
- **No per-user provider/pool restriction (v1).** A valid user key has the
  exact same access a valid shared-secret request has — every provider,
  every pool. The only behavioral difference is attribution in the logs.
  Scoping keys to specific providers/pools is a natural v2 if ever needed,
  but is out of scope here (see "Out of scope").

## Implementation decisions (2026-09-23)

Implemented on `feature/client-keys` (migration `0008_users.sql`,
`src/users/`, `frontend/src/pages/Users.tsx`). Where the code differs from
the sections below, **the code and this list win**:

- **Shared secret is attributed as `"admin"`, not `None`.** Otherwise
  shared-secret traffic is indistinguishable from anonymous open-access
  traffic (and from pre-feature log rows) in `request_log`. `admin` (and
  `anonymous`) are reserved and can't be used as a user id.
- **Key format** is `1r_<32 random bytes, hex>` rather than `usr_<base64url>`
  — the prefix marks it as a 1router credential; still no parsing
  significance.
- **`users` table** also has `key_prefix` (first 10 chars of the raw key,
  for telling keys apart in the UI) and `last_used_at` (refreshed at most
  once a minute, in the background). `name` is optional on create and
  falls back to the id.
- **`request_log.user_id`** is a single nullable column (plus an index on
  `(user_id, created_at)`); new `GET /admin/stats/users` aggregates by it.
- **Credential headers:** `Authorization: Bearer <key>` *or*
  `x-api-key: <key>` (Anthropic SDKs), for both user keys and the shared
  secret.
- **Open access:** an unknown/absent key is anonymous (`NULL`); an active
  user key is still attributed.
- **Rotate on a revoked user is `409`** (revocation stays one-way); unknown
  id is `404`; duplicate id on create is `409`.
- **Export/import includes `key_hash`** (the open question under
  "Export/import"): `ExportDump.users`, `#[serde(default)]` so older dumps
  and seed files still import.

## New table: `users`

```sql
CREATE TABLE users (
    id          TEXT PRIMARY KEY,   -- admin-chosen slug, same idiom as providers.id
    name        TEXT NOT NULL,      -- display label, not unique
    key_hash    TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMP NOT NULL,
    revoked_at  TIMESTAMP           -- NULL = active; set once, never cleared
);
```

- `id` follows the same admin-chosen-slug idiom as `providers.id`/`pools.id`
  (`validate_path_id` reused — see "Validation" below), not an autoincrement
  int, so it reads directly in `request_log`/`dataset_log` rows without a
  join.
- The raw key is shown to the admin **exactly once**, at creation — same UX
  as the existing "reveal admin secret once" convention implied by
  `.router_secret` being a sidecar file, not an admin-UI-visible value. Only
  `key_hash` is persisted; there is no "view existing key" endpoint, only
  "revoke and issue a new one."
- `revoked_at` is a soft-delete: a revoked user's key stops authenticating
  immediately, but the row (and its historical attribution in
  `request_log`/`dataset_log`, which store `user_id` as a bare string, not a
  foreign key) stays intact. No hard delete endpoint in v1 — revoke is
  sufficient and preserves log referential meaning.

## Key format and hashing

Keys are high-entropy random tokens the admin never has to type twice or
remember — the same shape as `shared_secret`/admin session tokens, not a
human password. That means **SHA-256 of the raw key**, matching
`admin/auth/session.rs`'s existing session-token hashing
(`session.rs:25`), not Argon2 (`admin/auth/password.rs`) — Argon2's
deliberate slowness defends against guessing a low-entropy human password;
it's wasted (and would add real per-request latency to every proxied call)
against a 256-bit random token where brute force is already infeasible.

Format: `usr_<32 random bytes, base64url-nopad>`, generated with the same
`rand`-backed approach `shared_secret` generation already uses. The `usr_`
prefix is a scanning aid (so a key accidentally committed to a repo or
pasted in a log is greppable / recognizable at a glance) — same idea as
Stripe/GitHub token prefixes — with no parsing significance; validation
still hashes the whole string including the prefix.

## Auth: extending `require_bearer`, not replacing it

`require_bearer` (`src/auth/middleware.rs:11-37`) currently does one
constant-shape check: does `Authorization: Bearer <token>` equal the current
shared secret. It gates all of `/v1/*` when `require_shared_secret` is on,
and is a no-op (open access) when it's off.

New resolution order inside the same middleware:

```
1. token == current shared_secret  → proceed, request has no user_id (None)
2. sha256(token) matches an active users.key_hash row → proceed, attach
   that user's id to the request via req.extensions_mut()
3. neither → 401, same as today
```

When `require_shared_secret` is `false` (open access), behavior is
unchanged from today for step 1's absence — the middleware still
short-circuits to `next.run(req).await` before checking anything. A user
key does **not** imply "require auth" the way the shared secret's presence
does; open-access mode remains a single global toggle. (This means in open
access mode, a client can still optionally send a valid user key to get
attribution, but isn't required to — matching "attribution, not access
control" from the goal.) This needs one small change from today's early
return: even in open-access mode, if a bearer token is present and matches
an active user key, still attach the `user_id` — attribution shouldn't
silently disappear just because the gateway happens to be configured wide
open. If no token or a non-matching token is present, proceed exactly as
today (anonymous, `user_id: None`).

Lookup is a `SELECT id FROM users WHERE key_hash = ? AND revoked_at IS
NULL` against the request's own token hash — one extra indexed query per
request when the shared-secret check (a cheap string compare) fails, so
the fast path (shared secret, or open access with no user key) pays
nothing extra. No in-memory cache in v1 (mirrors `request_log`'s "simple
first" bias); revisit only if this measurably shows up in latency.

`req.extensions_mut().insert(AuthenticatedUser { id })` (new small struct,
next to `admin::auth::session::AdminSession`) is how the id crosses from
middleware into `handle_proxy`, which extracts it via `Extension<Option<AuthenticatedUser>>`
(absent extension = shared-secret or anonymous open-access request).

## Threading `user_id` into the two existing log paths

Both `LogEntry` (`request_log`) and `DatasetLogEntry` (`dataset_log`)
already have — or need — a `user_id: Option<String>` field:

- `DatasetLogEntry.user_id` already exists (`dataset_log.rs`), currently
  hardcoded to `None` at construction (`flow.rs:86`). Change that one
  construction site to read the `AuthenticatedUser` extracted at the top of
  `handle_proxy`, threaded through the same way `request_id`/`timestamp`
  already are.
- `LogEntry` (`core/model.rs`, `request_log.rs`) has **no** `user_id` field
  today — add one (`Option<String>`), plus a matching nullable
  `request_log.user_id` column via a new migration, plus the column in the
  `INSERT` in `request_log.rs:52-64`. This is the "attribute the log that
  already exists for every request" half of the goal — dataset logging is
  opt-in and most exchanges never hit it, but `request_log` logs
  everything, so it's the table an admin actually queries to answer "what
  has user X been calling."

Both writes read from the same single `AuthenticatedUser` extracted once at
the top of `handle_proxy` (`flow.rs`'s existing `log()` helper at
`flow.rs:38` and the `DatasetLogEntry` construction at `flow.rs:80` both
gain one new parameter) — no second DB lookup, no re-deriving it per log
call.

## Admin API

New `src/users/` module (mirrors `src/providers/`'s
`mod.rs`/`queries.rs`/`routes.rs` split):

- `POST /admin/users` — `{ id, name }` → creates row, generates the raw key
  server-side, returns `{ id, name, key, created_at }` **once** (the only
  response that ever contains the raw key).
- `GET /admin/users` — list, `{ id, name, created_at, revoked_at }` per row
  — never `key_hash`, never the raw key.
- `POST /admin/users/:id/revoke` — sets `revoked_at = now()` if not already
  set; idempotent (revoking twice is a no-op 200, not an error).
- `POST /admin/users/:id/rotate` — same shape as create's response (new raw
  key shown once), replaces `key_hash` in place, `id`/`name`/`created_at`
  unchanged. This is how an admin issues a replacement without juggling a
  new `id`.

No `DELETE /admin/users/:id` in v1 — revoke is the only lifecycle op,
consistent with "soft-delete only" above.

All four routes sit behind the existing `require_admin_session` layer
(`src/app.rs`'s existing `/admin/*` nesting), same as every other admin
route — no new auth concept needed for the admin side, only for the `/v1/*`
proxy side.

`id` validation reuses `validate_path_id` (already used for
`providers`/`pools` ids) — reject empty and `/`, same rules, same function,
not a reimplementation.

## Export/import and seed compatibility

Same trap `dataset_logging` hit: `src/admin/mod.rs`'s `ExportDump` and
`src/seed.rs` serialize/deserialize config structs directly with their own
INSERT SQL. If `User` is added to either path, it needs `#[serde(default)]`
treatment for any new optional fields and an explicit decision about
whether exporting a user **includes** `key_hash` (so an export/import
round-trip preserves working keys) or deliberately omits it (so a shared
export file can't be used to impersonate users elsewhere). Recommendation:
include `key_hash` (it's already an irreversible hash, not the raw key —
same sensitivity class as `providers.api_key`, which the existing export
already includes) so import restores a fully working system, but this is
an explicit call-out for the implementation plan to confirm, not decided
by fiat here.

## Frontend (admin UI)

A new "Users" page, same list/create/revoke shape as the existing
`Providers.tsx` page: table of users (id, name, created_at, revoked
badge), a "Create user" form (id, name) whose submit response shows the
raw key **once** in a copy-to-clipboard dialog with an explicit "you won't
see this again" warning (mirrors how the onboarding wizard/Command Code
browser-login flow already handles one-time-visible secrets), and a
"Revoke"/"Rotate" action per row. No password field, no login flow — this
page only issues bearer keys for `/v1/*`, it has nothing to do with
`/admin/*` auth.

## Out of scope (v1)

- Per-user provider/pool access restriction — every valid user key has full
  `/v1/*` access, identical to the shared secret; only attribution differs.
- Session/login/logout lifecycle, login events, or a `/v1/login`-style
  endpoint for these keys — they are bearer credentials, not accounts.
- Rate limiting or usage quotas per user — `request_log` now has enough
  (`user_id` + existing `latency_ms`/`success`/timestamps) to build this
  later without another schema change, but no enforcement ships here.
- Hard delete of a user row — revoke only.
- An in-memory cache for the key-hash lookup — added only if profiling
  shows it matters.
- Any change to `require_shared_secret`'s existing semantics as the single
  global open-access toggle.
