# Pool member model identity — design

**Status:** approved for implementation, after a second Opus review found
the schema/SQL half sound but flagged a critical gap (now folded in below,
see "Runtime cooldown/circuit-breaker keying" — resolved as part of this
design, not deferred) plus several smaller corrections throughout.

## Problem

`pool_members`' primary key is `(pool_id, provider_id)`:

```sql
CREATE TABLE pool_members (
    pool_id     TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    priority    INTEGER NOT NULL,
    PRIMARY KEY (pool_id, provider_id)
);
-- 0003_pool_member_model_override.sql later adds: model_override TEXT
```

A provider can occupy only **one slot per pool**. But since
`0003_pool_member_model_override.sql`, a member's effective upstream call
is `(provider credentials, effective_model)` where `effective_model =
model_override.unwrap_or(provider.upstream_model)` (`src/pools/select.rs`).
The PK is a strict prefix of that real identity — a partial-key defect, not
a load-bearing constraint. `select_direct_provider` and `GET /v1/models`
already treat `(provider, model)` as the addressable unit; `pool_members`
is the only place left assuming one provider = one model.

**Concrete pain:** to round-robin across 3 Command Code models
(`deepseek/deepseek-v4-flash`, `deepseek/deepseek-v4-pro`,
`MiniMaxAI/MiniMax-M3`) sharing one OAuth account, a user must create 3
separate `providers` rows today, each pinned to one model, all pointed at
the same credential via `POST /admin/providers/:id/commandcode/key`.

**This is not just a UX wart — it's a live correctness bug.**
`provider_oauth_state` is `PRIMARY KEY provider_id`. Command Code's token
refresh (`src/providers/adapter/codex/refresh.rs`) rotates the refresh
token on every use. Three provider rows sharing one account means three
independent refresh chains racing on one upstream credential — whichever
refreshes last silently invalidates its siblings' stored tokens.
`refresh_lock.rs` locks per provider id, so it can't prevent this across
rows. The workaround that "solves" the round-robin UX problem actively
causes intermittent auth failures.

## History: was `(pool_id, provider_id)` wrong from the start?

No. At `0001_init.sql`, a provider was an indivisible credential+model pair
(`upstream_model NOT NULL`, no override). Under that model, listing the
same provider twice in a pool would mean "retry the identical upstream call
twice" — pure noise, correctly disallowed. The PK was right for the entity
as it existed then.

`0003` redefined the entity (effective model moved onto the membership row)
without re-deriving the key. This was not a scoped-out item in the
round-robin plan's "Out of scope (v1)" list (health-weighted rotation, a
`random` strategy, cross-instance cursor sharing, direct-addressing
rotation) — it simply wasn't noticed, because before round-robin, N members
of one credential differing only by model had a weak story (failover from
a model to a same-account sibling model, not a real fallback). Round-robin
is what makes the missing capability valuable, not what caused the defect.

## Options considered

**Option A — widen the member's identity to include the model**, via a
unique *expression* index rather than a nullable column in the PK:

```sql
CREATE UNIQUE INDEX idx_pool_members_identity
  ON pool_members (pool_id, provider_id, COALESCE(model_override, ''));
```

A raw `PRIMARY KEY (pool_id, provider_id, model_override)` is broken
because SQL treats distinct `NULL`s as non-equal in uniqueness checks, so
unlimited `(pool, provider, NULL)` duplicates would slip in. The
`COALESCE` expression collapses "no override" to one well-defined slot and
keeps `model_override IS NULL` meaning "inherit `providers.upstream_model`"
— `select.rs` needs no change. SQLite accepts an expression as an `ON
CONFLICT` target when it matches an expression index, so upserts stay
one-statement.

**Option B — surrogate `id INTEGER PRIMARY KEY AUTOINCREMENT`,** address
members by that id instead of `provider_id`. Rejected:

1. **Breaks export/import idempotency.** `src/admin/mod.rs`'s import
   upserts on `(pool_id, provider_id)` — a natural key, meaningful across
   databases. A surrogate id is meaningless across instances: re-importing
   the same dump into a fresh db either collides or duplicates every
   member row.
2. **Removes the only guard against silent duplication.** With no
   uniqueness at all, a double-clicked "Add" or a retried request creates
   a duplicate `(provider, model)` member, silently doubling that member's
   share of round-robin traffic — a routing bug with no error surfaced.
3. **Forces an API/UI reshape, not an extension.** `PUT
   /admin/pools/:id/members` is a natural-key upsert today, and the admin
   UI's reorder loop (`frontend/src/pages/Pools.tsx`) re-PUTs the full
   member array by `provider_id` after every drag. Under B this becomes
   POST-create + PUT-by-id everywhere; under A it keeps working unchanged.

B's one genuine advantage — two members with the *same* provider and *same*
model, i.e. integer weighting by repetition — is better served later by an
explicit `weight INTEGER NOT NULL DEFAULT 1` column, not by allowing
silently-duplicated rows.

**Option C — split credentials out of `providers`** (a `credentials` table
+ `providers.credential_id`), so N model-pinned providers can share one
OAuth chain safely without the identity problem ever arising. This is the
*correct long-term fix for the token-rotation hazard* described above, but
it touches `providers`, `provider_oauth_state`, both OAuth adapters, the
onboarding wizard, the Providers UI, and export/import — an order of
magnitude larger and orthogonal to the pool-membership fix. **Logged as a
follow-up, not coupled to this plan** (see `docs/superpowers/BACKLOG.md`).
Option A is forward-compatible with it: if credentials are ever split out,
`model_override` simply becomes unused and can be dropped.

## Decision

**Option A.** Smallest change that makes the schema match the domain,
preserves every property the current code depends on (natural-key upsert,
portable dumps, unchanged `select()`/rotation semantics), and needs zero
breaking API changes.

## Scope of the fix

### Schema (migration `0005`)

SQLite can't `ALTER TABLE` a primary key, so this is the standard rebuild:
`CREATE pool_members_new` (no declared PK, just the rowid), copy rows,
`DROP` old table, `RENAME`, recreate `idx_pool_members_pool` and the new
`idx_pool_members_identity` unique expression index. Existing rows migrate
untouched — every current row has a distinct `(pool_id, provider_id)`,
hence a distinct `(pool_id, provider_id, COALESCE(model_override,''))>`.
No data reinterpretation, no default backfill needed.

`sqlx`'s migration runner executes each file inside a transaction
(`src/core/db.rs`), and the connection has `PRAGMA foreign_keys` set — that
pragma cannot be toggled inside a transaction, so the rebuild must never
need FKs disabled (it doesn't: parent rows already exist throughout).

### Query layer (`src/pools/queries.rs`, `src/admin/mod.rs`, `src/core/state.rs`)

Three `ON CONFLICT(pool_id, provider_id)` sites retarget to `ON CONFLICT
(pool_id, provider_id, COALESCE(model_override, ''))`:
- `queries::upsert_member` (`DO UPDATE SET priority = excluded.priority`
  only now — updating `model_override` in the `SET` is meaningless once
  it's part of the conflict target, since a conflict means it already
  equals the excluded value)
- `src/admin/mod.rs` import (same `SET priority` narrowing)
- `src/core/state.rs::ensure_direct_pools_for_unassigned_providers`'s `DO
  NOTHING` target

Both member-loading queries (`queries::list_members`,
`state::load_snapshot`) add a deterministic tiebreak: `ORDER BY priority
ASC, provider_id ASC, COALESCE(model_override, '') ASC`. This is a real
correctness fix, not cosmetic: `select()`'s `sort_by_key(|m| m.priority)`
is stable, but SQL doesn't guarantee row order for tied `priority` values
without an explicit tiebreak. Once one provider can have several
same-priority members, an unstable base order means a `RoundRobin` pool's
cursor rotates over a list whose order can silently shift between config
reloads.

### Rotation cursor (`src/pools/select.rs`) — no change

`rotate_from_cursor` is keyed by `pool.id`, indexes `% members.len()`, and
rotates a `Vec` — it never inspects `provider_id`. More members, including
same-provider ones, just means a longer cycle; the existing `% len` guard
already covers list-length changes under a live cursor.

### Runtime cooldown/circuit-breaker keying (`src/proxy/flow.rs`,
`src/core/runtime.rs`, `src/providers/routes.rs`,
`src/providers/oauth_routes.rs`, `src/providers/refresh_task.rs`) — **must
change**

**This was the critical finding of the second review, and it is a
correctness bug, not a nice-to-have.** `AppState.runtime` (the per-provider
cooldown/`Misconfigured` map) is keyed by `provider.id` alone at every one
of ~20 sites in `flow.rs`'s failover loop
(`state.runtime.entry(provider.id.clone())`). Once one provider can back
several pool members with different `model_override`s, this has two
compounding effects, both the *opposite* of what the first draft of this
doc claimed:

- **No intra-request failover between same-provider members.** If
  member 1 (`deepseek-v4-flash`) 5xxs, the cooldown is recorded against
  the provider id. When the loop reaches member 2 (`deepseek-v4-pro`,
  same provider, different model), `is_available()` is already `false` and
  it's `continue`d past — not tried. The request 503s instead of failing
  over to a sibling model, even though the sibling is a fully independent
  upstream call.
- **One model's `NonRetryable` error poisons every model on that
  credential.** A `400`/`404` for one model name (e.g. temporarily wrong
  after an upstream rename) calls `mark_misconfigured()` on the provider
  id, which `is_available()` treats as permanently down with no expiry —
  taking all 3 models offline until an admin manually clears it
  (`validate-model`, a provider update, or an OAuth callback).

(The first draft of this document said the failover tail "burns through"
same-credential siblings before reaching a different provider — that is
backwards; it *skips* them via the shared cooldown, which is strictly
worse.)

**Fix, in scope for this plan:** widen the runtime-state key from
`provider.id` to `(provider.id, effective_model)` — e.g. a
`runtime_key(provider_id: &str, model: &str) -> String` joining with a
separator that can't appear in either half (both are validated path-id-like
strings; `\u{1f}` unit-separator is a safe choice), used at every
`state.runtime.entry(...)` call site in `flow.rs`. The three places that
read/reset runtime state *by provider id alone* (status display in
`providers/routes.rs`, the `reset_to_healthy()` calls in
`providers/routes.rs` and `oauth_routes.rs` after a credential update, and
`refresh_task.rs`'s background refresh loop) become prefix
iterations/aggregations over `state.runtime` instead of a single
`get_mut(&id)` — e.g. "worst status across all `(id, *)` keys" for display,
"reset every `(id, *)` entry" for the post-credential-fix clear. This is a
contained, mechanical change (one key-building helper + call-site updates),
not a redesign — `RotationState`/`rotate_from_cursor` above are untouched,
this is purely about the failure-tracking map.

This directly fixes the target scenario: a transient 500 on one model no
longer cools down its siblings, and a genuinely broken model can be
isolated (via its own `Misconfigured` flag) without taking its
credential-mates offline. It also means the account-level failure mode
this doc originally worried about (a revoked token affecting all models)
now correctly cools down *every* `(provider_id, *)` key roughly together
(each fails independently but on the same timescale, since they share the
same underlying credential) rather than either all being needlessly shared
or, after this fix, incorrectly appearing independent when the underlying
cause (a dead credential) is not.

### Admin API (`src/pools/routes.rs`) — additive only

- `PUT /admin/pools/:id/members` — **unchanged wire shape.** Identity
  becomes `(provider_id, model_override)`: same provider + same model
  updates priority in place; same provider + different model inserts a
  new member. Every existing caller that never sends two models for one
  provider sees no behavior change.
- `DELETE /admin/pools/:id/members/:provider_id` — **keep the route,** add
  an optional `?model=<name>` query param. With `model` present, delete
  that exact `(provider_id, model)` member. Without it, delete every
  member for that provider in the pool (identical to today's behavior
  whenever there's only one). An empty `?model=` means "the member with no
  override" (`model_override IS NULL`), distinct from omitting the param
  entirely — document this explicitly, it's the one API subtlety.
- No new fields on `PoolMember`'s JSON shape. "Edit an existing member's
  model in place" is out of scope for v1 — there's no such UI affordance
  today (`addMember` always assigns a new `priority = max + 1`, never
  edits an existing row's `model_override`), and today's "PUT the same
  provider_id with a different model_override overwrites the old row" (an
  implicit edit path nothing currently exercises) becomes "PUT inserts a
  second member" after this fix. If an explicit "change this member's
  model" UI is wanted later, it needs both old and new model in one
  request (e.g. `PATCH` with `from`/`to`); noted for follow-up, not
  designed here since nothing depends on it today.

### Admin UI (`frontend/src/pages/Pools.tsx`)

`member.provider_id` alone is currently used as: the `SortableContext`
item id, the `<li key>`, the delete-button key, and the lookup key in
`moveMember`/`onDragEnd` (`findIndex(m => m.provider_id === id)`). All four
must switch to a composite key — `` `${provider_id}\u0000${model_override
?? ""}` `` — or duplicates will silently reorder/delete the wrong row.
`recomputeMemberPriorities` and the reorder-PUT loop need no logic change;
each PUT already round-trips that row's own `model_override`.

Two UX improvements ride along naturally, since they're what motivated the
fix:
- Populate the "add member" model field from `GET
  /admin/providers/:id/list-models` as a `<select>`/datalist instead of
  free text — the `ModelFetchState` plumbing already exists for the
  validation check, this is wiring, not new machinery.
- Don't clear `providerId` after a successful add, only `modelOverride` —
  turns "add flash, then pro, then minimax from the same provider" into
  one provider pick + three model picks, instead of re-selecting the
  provider three times.

## Production database risk

`sqlx::migrate!` runs on every startup with no dry-run path
(`src/core/db.rs`). The first launch of a `0005`-containing binary against
`E:\1router\1router.db` rebuilds `pool_members` unprompted, and there are
no down-migrations — rollback means restoring a backup, not reverting a
migration. Per this project's standing rule, **do not run this build
against the real production db without an explicit go-ahead and a backup
taken first** (copy `.db` + `-wal` + `-shm` together with the gateway
stopped, or `sqlite3 .backup`/`VACUUM INTO`). Verify against a copy first:
row-count equality pre/post the rebuild, plus `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` clean.

## Out of scope

- Option C (credential/provider split) — logged to
  `docs/superpowers/BACKLOG.md` as a follow-up.
- Explicit "edit member's model" API/UI affordance — noted above, not
  needed by anything today.
- Weighted round-robin (`weight` column) — noted as the correct way to get
  Option B's one advantage (same provider+model repeated), not requested
  by anything today.
- A pre-existing, unrelated bug found while reading `src/admin/mod.rs`'s
  import path: `import_config` inserts pools as `(id, wire_format,
  created_at)` and only updates `wire_format` on conflict — it silently
  drops `strategy`/`sticky_limit` on import, so exporting and
  re-importing a round-robin pool resurrects it as `priority`. Not caused
  by, or fixed by, this plan (it predates it, from the round-robin work);
  logged to `docs/superpowers/BACKLOG.md` since this plan's Task 2 edits
  a statement three lines away from it and would otherwise be an easy
  place to have silently "fixed by proximity" without a test.
- `?model=` absent on `DELETE .../members/:provider_id` deleting *every*
  member for that provider in the pool is a deliberate, if sharp, default
  — kept only for backward compatibility with existing callers that
  predate multi-model members. The admin UI always sends `?model=`
  (possibly empty) for its per-row delete button, so this default is
  never hit through the UI; it only matters for direct API callers, who
  are relying on today's documented single-model-per-provider behavior
  anyway.
