# Open access mode — Design

## Goal

Let an operator turn OFF the shared-secret requirement on `/v1/*` client
requests — "open access" — toggleable via `1router setup` and the admin UI
Settings page, hot-swappable without a restart. `/admin/*` auth is
completely unaffected: it keeps its own admin password/session, and the
shared secret keeps working as an alternate admin Bearer credential either
way (see "Scope", below).

Default: **open** for any install that has never had this setting decided
before, in every boot mode (interactive and headless alike). Any install
that already resolved a shared secret before this feature existed keeps
requiring it, unchanged, on upgrade — see "Migration safety" for exactly how
those two defaults are told apart.

## Naming

**"Open access"** (mode), with **positive internal polarity**:
`require_shared_secret: bool`. Never introduce a `no_shared_secret` /
`open_access: bool` pair pointing the opposite way — a double negative like
"open access: false" is how this gets flipped backwards in a config file.

Rejected: "No Shared Secret mode" (unreadable as a toggle state — "No
Shared Secret: OFF"), "Local mode" (misleading: `ROUTER_LISTEN_ADDR`
defaults to `0.0.0.0`, so this doesn't imply local at all), "Anonymous
access" (IAM-flavored overclaim).

## Scope: `/v1/*` only, `/admin/*` untouched

Only `auth::middleware::require_bearer` (guarding the proxy routes) grows a
branch. `require_admin_session` is **not modified**: it keeps requiring a
session cookie or a valid Bearer of the shared secret, regardless of open
access. This means:

- The shared secret is **never dead weight**, even in open mode — it still
  authenticates `/admin/*` via Bearer. The admin UI must not imply
  otherwise (see "Admin UI").
- A regression test must assert `GET /admin/providers` with no credential
  still 401s when open access is on — the one check that would catch a
  copy-paste mistake that widens the wrong middleware.

## Persistence: the existing (unused) `server_secrets` table

`migrations/0002_admin_ui.sql` already defines:

```sql
CREATE TABLE server_secrets (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

No code references it today. Reuse it — **no new migration** — with a
single row: `name = 'require_shared_secret'`, `value = 'true' | 'false'`.
This is a clean fit: not a secret (no 0600/atomic-write need like
`.router_secret`), and the DB is already the source of truth for
providers/pools/admin users, loaded well after the point (`build_router`)
where this flag is needed.

Add a thin `core::settings` module:

```rust
pub async fn get_bool(db: &SqlitePool, name: &str) -> anyhow::Result<Option<bool>>;
pub async fn set_bool(db: &SqlitePool, name: &str, value: bool) -> anyhow::Result<()>;
```

(`INSERT ... ON CONFLICT(name) DO UPDATE`, storing `"true"`/`"false"` and
erroring on any other stored value — never guess.)

## Env var: `ROUTER_REQUIRE_SHARED_SECRET`

- Accepts `true|false|1|0|yes|no`, case-insensitive. Positive polarity,
  matching `require_shared_secret` — `ROUTER_NO_SHARED_SECRET=false` is
  exactly the double-negative trap this design avoids elsewhere.
- **Set → wins over the DB row, unconditionally.** Mirrors
  `ROUTER_SHARED_SECRET`'s precedence over the sidecar file. The admin
  `PATCH` 409s while it's set, same shape as the existing
  `SecretOrigin::Env` guard in `admin/settings.rs`.
- **Unparseable value → fail fast at boot** (`anyhow::bail!`), never
  silently default a security switch on a typo.
- **Unset → resolved per "Default resolution" below.**

## Default resolution (the one subtle part)

Both "open by default for every fresh install, interactive or headless"
and "never flip an existing install's behavior on upgrade" have to hold at
once. They can, because `main.rs` already knows, at the point it resolves
the shared secret, whether this box has ever had one before
(`config::resolve_shared_secret` returns `SecretSource::Env`,
`SidecarFile`, or `BootstrapNeeded`) — `BootstrapNeeded` *is* "fresh
install," precisely.

Resolved once, in `main.rs`, right after `init_pool`:

1. `ROUTER_REQUIRE_SHARED_SECRET` set → that value. Origin `Env`.
2. Else, a `server_secrets` row exists → that value. Origin `Db`.
3. Else (no row yet — this box has never decided) →
   - the shared secret resolved as `SecretSource::BootstrapNeeded` this
     boot (truly first-ever boot, no env secret, no sidecar file yet) →
     **default `false` (open)**, in every boot mode, interactive or
     headless. Persist it as an explicit row so step 2 answers from here on.
   - the shared secret resolved as `Env` or `SidecarFile` (this box
     already had a secret before this feature existed — an
     upgrade) → **default `true` (required)**, persisted the same way.

Consequence: **an existing deployment's `/v1/*` auth is bit-for-bit
unchanged by this feature.** That's the property to write the regression
test for (see "Testing").

`1router setup`'s interactive Settings flow, and the admin UI, can flip
either default afterward at any time (subject to the `Env` origin guard).

## Non-loopback guardrail

`ROUTER_LISTEN_ADDR` defaults to `0.0.0.0:8080`, not loopback — "open
access" on the default bind is reachable from the whole LAN, not just the
operator's own machine. The real risk being guarded against: an
unauthenticated `/v1/*` is a free relay for whatever provider credentials
are configured — anyone who can reach the port spends the operator's
API budget, attributably to nobody.

Add `core::config::listen_addr_is_loopback(&SocketAddr) -> bool`
(`127.0.0.0/8` or `::1`).

Never block — only escalate friction with exposure:

| State | CLI confirmation | Boot log | Admin banner |
|---|---|---|---|
| required | none | (existing default-secret checks only) | (existing checks only) |
| open + loopback | `y/N`, default No | one `INFO` line | banner shown, non-critical style |
| open + non-loopback | must type `OPEN` verbatim | `WARN` every boot | banner shown, critical style |

Boot warning (next to the existing default-secret/default-password ones in
`main.rs`), only when open **and** non-loopback:

```
WARN open access is ON: /v1/* accepts requests with no API key, and this
     gateway is listening on 0.0.0.0:8080 — reachable from other machines.
     Anyone who can reach it can spend your provider credits. Set
     ROUTER_LISTEN_ADDR=127.0.0.1:8080, or ROUTER_REQUIRE_SHARED_SECRET=true.
```

## Import/export

`POST /admin/import` / `GET /admin/export` (whatever the current
providers/pools JSON covers) must **not** be able to change this setting —
excluded entirely from both. A config file must never be able to disable
authentication.

## Admin UI

### New endpoints, mirroring `admin/settings.rs`'s existing shape

- `GET /admin/settings/auth-mode` → `{ require_shared_secret: bool, origin: "env"|"db"|"default" }`
- `PATCH /admin/settings/auth-mode` body `{ require_shared_secret: bool }` →
  same shape; `409 Conflict` when origin is `Env` (identical wording style
  to the existing `patch_shared_secret` guard).
- `GET /admin/settings/security-status` grows two fields:
  `require_shared_secret: bool`, `listen_addr_is_loopback: bool`.

### `Settings.tsx` placement

New section **"Client API access"**, inserted between "Admin password" and
the existing "Shared secret" form — the mode control governs whether that
form matters, so it reads top-to-bottom in the right order:

```
Admin password            (unchanged)
Client API access         ← new: the /v1 access mode radio pair
Shared secret              (existing form; disabled+annotated when open — see below)
Connect a client          (unchanged except the curl snippet, see below)
```

Two-option radio group (not a checkbox — the states have real names and a
checkbox would need a negative label):

```
/v1 access mode
( ) API key required — clients send Authorization: Bearer <key>
(•) Open access — /v1/* accepts requests with no API key
    The admin UI still requires this password. Anyone who can reach this
    gateway can send requests through your providers.
```

Switching to Open when `!listen_addr_is_loopback` requires an inline
confirm step (a second "Yes, enable open access" click), not a bare
`window.confirm`.

**Do not hide the shared-secret form when open — disable it and annotate
it.** It is still a live `/admin/*` Bearer credential in open mode (see
"Scope"), so hiding it would misrepresent what it does. Add the note: *"Not
required for `/v1/*` while open access is on. Still accepted as a Bearer
token for `/admin/*` — keep it secret."*

The "Connect a client" `curl` snippet drops the
`-H "Authorization: Bearer …"` line when open, replaced with a one-line
comment `# no API key needed — open access is on`.

### `SecurityBanner` (`App.tsx`)

Show an indicator whenever open access is on (both loopback and
non-loopback — per product decision, don't suppress it for the common
local case), escalating severity for non-loopback:

- open + loopback → same non-critical `role="alert"` style as the existing
  default-secret/password nags: *"Open access is on: `/v1/*` accepts
  requests with no API key. Change this on the Settings page."*
- open + non-loopback → a **critical** variant (distinct styling/wording):
  *"Open access is on and this gateway isn't bound to localhost — anyone
  who can reach it can send requests through your providers with no
  credentials. Restrict `ROUTER_LISTEN_ADDR` to `127.0.0.1` or require an
  API key on the Settings page."* Rendered above the existing
  default-credential warnings.

## CLI: `1router setup` menu restructure

`onboarding::run_wizard` currently serves both the first-boot auto-trigger
and manual `1router setup` as one linear flow. Split it:

- **First-boot auto-trigger** (`main.rs`'s existing
  `providers_table_is_empty` gate) keeps the linear flow — a first-time
  user should be walked straight to a working provider, not handed a menu.
  It gains one step: the access-mode prompt, inserted right after
  `resolve_or_prompt_secret`, defaulted to Open (per "Default resolution"),
  shown with the loopback/non-loopback wording from the guardrail table.
- **`1router setup` (manual re-run)** now shows a top-level menu instead of
  going straight into "add a provider?":

```
=== 1router setup ===
  listening on 0.0.0.0:8080   db: 1router.db
  2 providers · 3 pools · /v1 access: open (no API key)

? What do you want to do?
❯ Providers   — add or review upstream providers
  Pools       — map the `model` names clients request to providers
  Settings    — API key, access mode, admin password
  Connection details — base URL, model names, example request
  Quit
```

  Ordering mirrors the admin nav (Providers, Pools, Settings) on purpose,
  so the two surfaces teach each other. "Providers"/"Pools" wrap today's
  existing provider/pool-adding loop unchanged; "Connection details" is
  today's closing example, promoted to a reachable menu entry instead of
  only printing once at the end of a run.

  **Settings** submenu:

```
? Settings                                      (Esc to go back)
❯ /v1 access mode          — currently: open (no API key required)
  API key (shared secret)  — show or change
  Admin UI password        — change
  Back
```

  - `API key` wraps `resolve_or_prompt_secret` plus a new
    show/change flow, giving the CLI parity with the admin UI's
    reveal/rotate.
  - `Admin UI password` calls the existing `reset_admin_password`.
  - `/v1 access mode` shows the two-option `Select` (mirroring the admin
    UI's radio pair), with the same loopback/non-loopback confirmation
    (`y/N` vs. type `OPEN`) as the guardrail table specifies. When origin
    is `Env`, print the reason and return without prompting.

`dialoguer::Select::interact_opt()` (not `interact()`) throughout the new
menus, so Esc/Ctrl-C returns to the parent menu instead of erroring.

## Testing

The two tests that matter most (they're the ones that would catch a real
regression, not just exercise new code):

1. **Upgrade regression**: an existing `.router_secret` (or
   `ROUTER_SHARED_SECRET`) and no `server_secrets` row →
   `/v1/chat/completions` with no `Authorization` header still 401s.
2. **Scope**: open access on → `/v1/chat/completions` with no
   `Authorization` succeeds (routes to a stub/fake provider) →
   `GET /admin/providers` with no credential still 401s.

Plus: fresh-bootstrap defaults to open; `ROUTER_REQUIRE_SHARED_SECRET` set
→ `PATCH /admin/settings/auth-mode` 409s; garbage env value → boot
`Err`s; PATCH takes effect on the next request without a restart (the
`AtomicBool`/hot-swap plumbing).

## Non-goals (v1)

- No rate limiting added for `/v1/*` in open mode — open access does not
  imply a safety net against abuse volume, only against the *credential*
  requirement. Call this out in the README so nobody assumes otherwise.
- No change to `/admin/*` auth of any kind.
- No change to export/import beyond excluding this one field.
