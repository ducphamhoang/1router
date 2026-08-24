# Pool Member Model Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one provider occupy several slots in the same pool, each with
a different `model_override`, so round-robin (and priority/failover) can
span several models from one credential (e.g. one Command Code OAuth
account serving `deepseek/deepseek-v4-flash`, `deepseek/deepseek-v4-pro`,
`MiniMaxAI/MiniMax-M3`) without the user creating a throwaway `providers`
row per model. Today's `PRIMARY KEY (pool_id, provider_id)` forbids this;
the throwaway-provider workaround it forces is not just awkward but an
active bug (see design doc) — three provider rows sharing one OAuth
account race independent token-refresh chains against each other.

**Design reference:**
`docs/superpowers/specs/2026-08-24-pool-member-model-identity-design.md`
— read it first. It has the full options analysis (Option A chosen over a
surrogate-id approach and over splitting credentials out of `providers`)
and, critically, the **runtime cooldown/circuit-breaker keying fix**
(Task 3 below) that a first-draft version of this plan omitted — without
it, widening pool membership as planned would make same-credential
multi-model pools *less* reliable than today's workaround, not more,
because a transient failure on one model would cool down (or permanently
misconfigure) every sibling model sharing that provider id.

**Architecture:** Two independent widenings, both required for this to
actually work end to end:
1. **Storage identity**: widen a pool member's real identity from
   `provider_id` to `(provider_id, model_override)`, via a SQLite unique
   *expression* index (`COALESCE(model_override, '')`) rather than a raw
   column in the primary key, so `NULL` (= inherit
   `provider.upstream_model`) correctly collapses to one slot.
2. **Runtime failure-tracking identity**: widen `AppState.runtime`'s key
   from `provider_id` to `(provider_id, effective_model)`, so a failure on
   one model doesn't cool down or misconfigure its siblings.
`select()`'s rotation math (`rotate_from_cursor`) needs no change for
either — it only ever deals with `Vec<PoolMember>`/cursor indices and
never looks at `provider_id` or touches runtime state.

**Tech Stack:** Existing deps only — no new crate. SQLite feature used:
unique index on an expression (`COALESCE(...)`), confirmed live against
this project's bundled SQLite (via `libsqlite3-sys`, currently 3.46.x,
well above the 3.24 minimum for expression-index upsert targets) during
the design review.

## Global Constraints

- Package is `router`, binary is `1router`. Build/test with `cargo build
  --offline` / `cargo test --offline`.
- **SQLite cannot `ALTER TABLE` a primary key or drop a `PRIMARY KEY`
  clause.** This migration is the standard 12-step rebuild: create
  `pool_members_new` with the desired shape, copy rows, drop the old
  table, rename, recreate indexes. Do this in one migration file; `sqlx`
  runs each migration file (plus its `_sqlx_migrations` bookkeeping) in a
  single transaction (`sqlx-sqlite`'s `Migrate::apply`, confirmed during
  review — there is no `-- no-transaction` escape for the SQLite driver).
- **`PRAGMA foreign_keys` cannot be toggled inside a transaction** (a
  `PRAGMA foreign_keys=OFF` inside a migration would be a **silent
  no-op**, not an error — confirmed during review). The rebuild must never
  require disabling FK checks — it doesn't, since every row being copied
  already has valid `pool_id`/`provider_id` parents.
- **Tasks 1 and 2 land as one atomic change** — between them the binary is
  broken (every `upsert_member`, the admin import, and
  `ensure_direct_pools_for_unassigned_providers` at boot fail with a
  SQLite "`ON CONFLICT` clause does not match any PRIMARY KEY or UNIQUE
  constraint" error) once Task 1's migration has run but Task 2's query
  retargeting hasn't landed. Never commit/release Task 1 without Task 2.
- **`COALESCE(model_override, '')` must appear verbatim** (same casing,
  same expression) at every `ON CONFLICT` site — SQLite matches conflict
  targets by *parsed expression structure*, and confirmed during review:
  `ifnull(...)` does **not** match a `coalesce(...)` index (fails with the
  same "does not match" error) even though they're semantically
  equivalent. Don't "clean up" the spelling at any one site.
- **Task 3 (runtime-state keying) is required, not optional** — see
  design doc's "Runtime cooldown/circuit-breaker keying" section. Without
  it, Tasks 1/2/4/5 ship a UI/API/schema that lets you build multi-model
  pools whose actual failover behavior is broken (same-provider siblings
  can't fail over to each other; one model's error can misconfigure all
  of them).
- **Never run a migration-0005-containing build against the real
  production db** (`E:\1router\1router.db`, per `CLAUDE.md` and this
  session's established convention) without explicit user go-ahead and a
  verified backup (`.db` + `-wal` + `-shm` together, gateway stopped, or
  `sqlite3 .backup`/`VACUUM INTO`). All manual verification in this plan
  happens against a **throwaway db** via `ROUTER_SQLITE_PATH` +
  `ROUTER_LISTEN_ADDR` env vars, never the real instance on port 8080.
- No new `AppState` field, so none of the six `AppState { }` construction
  sites (`src/main.rs`, `tests/common/mod.rs`, `tests/admin_pools.rs`,
  `tests/admin_settings.rs`, `tests/health_stats.rs`,
  `tests/open_access.rs`) need touching.
- Out of scope (v1, see design doc "Out of scope"): Option C
  (credential/provider split), explicit "edit member's model in place" API,
  a `weight` column, the pre-existing `import_config` strategy/sticky_limit
  drop bug (logged to `BACKLOG.md`, not fixed here).

---

### Task 1: Migration — rebuild `pool_members` with the new unique index

**Files:**
- Add: `migrations/0005_pool_member_model_identity.sql`
- Add test: a new `#[cfg(test)]` module (e.g. in `src/core/db.rs` or a new
  `tests/migration_0005.rs`)

```sql
-- pool_members' PK (pool_id, provider_id) assumed one provider = one model
-- per pool. Since 0003 added model_override, a member's real identity is
-- (pool_id, provider_id, model_override) - the PK was a partial key left
-- behind. Widen it via a unique EXPRESSION index (not a raw column in the
-- PK) so NULL model_override ("inherit provider.upstream_model")
-- correctly collapses to one slot instead of SQL's usual "distinct NULLs
-- never collide" behavior. See docs/superpowers/specs/
-- 2026-08-24-pool-member-model-identity-design.md for the full rationale.
--
-- This table intentionally has NO declared PRIMARY KEY after this
-- migration - uniqueness lives in idx_pool_members_identity below. Do not
-- "restore" a composite PK here; that reintroduces the bug this fixes.

-- Normalize the '' vs NULL sentinel BEFORE the rebuild: a literal empty
-- string model_override (possible today via a client sending
-- `"model_override": ""`, which deserializes to Some("")) would collide
-- with a real NULL row under the new index. Collapse any pre-existing ''
-- to NULL so the sentinel is unambiguous going forward - see the two
-- write-site filters added in Task 2 that keep it that way.
UPDATE pool_members SET model_override = NULL WHERE model_override = '';

CREATE TABLE pool_members_new (
    pool_id       TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
    provider_id   TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    priority      INTEGER NOT NULL,
    model_override TEXT
);

INSERT INTO pool_members_new (pool_id, provider_id, priority, model_override)
SELECT pool_id, provider_id, priority, model_override FROM pool_members;

DROP TABLE pool_members;
ALTER TABLE pool_members_new RENAME TO pool_members;

CREATE INDEX idx_pool_members_pool ON pool_members(pool_id);
CREATE UNIQUE INDEX idx_pool_members_identity
  ON pool_members (pool_id, provider_id, COALESCE(model_override, ''));
```

- [ ] **Step 1: Write the migration file** as above.
- [ ] **Step 2: Write a real pre-/post-migration test**, not a
  post-migration-only insert (a prior draft of this step inserted rows
  *after* all migrations had already run, which never exercises the
  rebuild's `INSERT … SELECT` at all — caught in review). Concretely:
  build a fresh temp-file db, apply migrations `0001`–`0004` only (e.g. via
  `include_str!` of each file run in order, or by running the full
  migrator and then constructing the pre-0005 fixture data — either way,
  the fixture rows must exist *before* 0005 runs), insert fixture rows
  including **at least one row with a non-NULL `model_override`** (0003
  already allows this) and **one pool with `strategy='round_robin'`**
  (0004), then apply `0005`, then assert:
  - row count and full row-set equality (not just count) between
    pre-migration and post-migration `pool_members`
  - `PRAGMA foreign_key_check` returns no rows
  - `PRAGMA integrity_check` returns `ok`
  - the new `idx_pool_members_identity` unique index actually rejects a
    duplicate `(pool_id, provider_id, model_override)` insert and accepts
    a same-provider insert with a *different* `model_override`
- [ ] **Step 3: Run to verify it passes:** the new test target, plus
  `cargo build --offline` (confirms `sqlx::migrate!` doesn't panic against
  a fresh db at binary startup/test-harness time).

---

### Task 2: Retarget every `pool_members` write/read site + fix the sentinel + existing test

Review found 3 `ON CONFLICT` sites (matching the first draft) **plus 3
more call sites the first draft missed**, and one existing test that would
otherwise silently stop testing what it claims to.

**Files:**
- Modify: `src/pools/queries.rs` (`upsert_member`, `list_members`, the
  `pool_and_member_crud` test)
- Modify: `src/admin/mod.rs` (import upsert)
- Modify: `src/core/state.rs` (`ensure_direct_pools_for_unassigned_providers`,
  `load_snapshot`)
- Modify: `src/onboarding.rs` (`assign_to_pool` — uncovered by the first
  draft; only needs a behavior-change note + its existing tests re-run,
  see Step 6)
- Modify: `src/providers/queries.rs` (the pool-membership `COUNT(*)` guard
  around line 92 — uncovered by the first draft)
- Modify: `src/pools/routes.rs` (`put_member`'s handler doesn't change
  logic, but confirm its call into `upsert_member` still compiles after
  Step 1's `SET` clause change — it does, this is a query-string-only
  change)

- [ ] **Step 1: `src/pools/queries.rs::upsert_member`** — change:
  ```rust
  "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES (?, ?, ?, ?)
   ON CONFLICT(pool_id, provider_id) DO UPDATE SET priority = excluded.priority, model_override = excluded.model_override"
  ```
  to:
  ```rust
  "INSERT INTO pool_members (pool_id, provider_id, priority, model_override) VALUES (?, ?, ?, ?)
   ON CONFLICT (pool_id, provider_id, COALESCE(model_override, '')) DO UPDATE SET priority = excluded.priority"
  ```
  Also normalize the sentinel at the write boundary here (belt-and-braces
  with Task 1's one-time cleanup and Step 5 below):
  before binding, do `let model_override = m.model_override.as_deref().filter(|s| !s.is_empty());`
  and bind that instead of `&m.model_override` directly, so a client
  sending `model_override: ""` is stored as `NULL`, not a literal `''`
  that would collide with a real no-override row. **Note on the `SET`
  clause:** dropping `model_override = excluded.model_override` is
  correct, but *not* for the reason "a conflict means it already equals
  excluded" — that's false in exactly the `''`-vs-`NULL` case this step
  just closed off. The real reason to drop it: once normalized, keeping it
  in `SET` would be a no-op on a true match and is simply redundant with
  the conflict target; removing it makes that invariant visible in the
  code.
- [ ] **Step 2: `src/pools/queries.rs::list_members`** — add a
  deterministic tiebreak so tied-priority members (increasingly likely
  once one provider has several same-priority entries) don't silently
  reorder between reloads, which would make a `RoundRobin` pool's cursor
  rotate over a shifting list:
  ```rust
  "SELECT pool_id, provider_id, priority, model_override FROM pool_members
   WHERE pool_id = ? ORDER BY priority ASC, provider_id ASC, COALESCE(model_override, '') ASC"
  ```
- [ ] **Step 3: `src/core/state.rs::load_snapshot`** — same `ORDER BY`
  change to the per-pool member query inside the loop.
- [ ] **Step 4: `src/core/state.rs::ensure_direct_pools_for_unassigned_providers`**
  — retarget its `ON CONFLICT(pool_id, provider_id) DO NOTHING` to `ON
  CONFLICT (pool_id, provider_id, COALESCE(model_override, '')) DO
  NOTHING`. Confirmed during review that this statement's existing `WHERE
  NOT EXISTS` clause already satisfies SQLite's INSERT-SELECT-plus-upsert
  parsing requirement — don't remove it. Behavior is unchanged (it always
  inserts `model_override = NULL` for a fresh shadow pool), this is purely
  so the statement doesn't fail against the new schema.
- [ ] **Step 5: `src/admin/mod.rs` import upsert** — same retarget as Step
  1 (drop `model_override` from `SET`, apply the same `filter(|s|
  !s.is_empty())` normalization to the imported value before binding).
- [ ] **Step 6: `src/onboarding.rs::assign_to_pool`** — no logic change
  needed (it already calls `list_members` + `upsert_member`, which stay
  correct), but **note the behavior change**: running `1router setup`
  twice for the same provider assigned to the same pool with two
  *different* `model_override`s now creates a second member instead of
  updating the first one in place (this is the intended new behavior, not
  a regression — it mirrors the admin API). Re-run
  `onboarding.rs`'s existing tests (`assign_appends_behind_existing_members`,
  the `assign_with_override_...` cases) to confirm they still pass
  unchanged (they test single-assignment scenarios that don't hit the new
  branch).
- [ ] **Step 7: `src/providers/queries.rs` wire-format-flip guard** (the
  `SELECT COUNT(*) FROM pool_members pm JOIN pools ...` around line 92) —
  change to `SELECT COUNT(DISTINCT pm.pool_id) FROM ...`. Without this,
  the guard's error message ("is a member of {N} pool(s)") would count
  *memberships* instead of *pools* once one provider can have several
  memberships in the same pool — e.g. a provider with 3 same-pool
  memberships would wrongly report "3 pool(s)" instead of "1 pool(s)".
- [ ] **Step 8: Fix `src/pools/queries.rs`'s existing `pool_and_member_crud`
  test.** As written today it upserts the same `(pool_id, provider_id)`
  twice with two different `model_override`s and asserts the second
  upsert updated the row in place — that assertion describes the *old*
  semantics this plan removes. Left unmodified, it would **silently keep
  passing** post-fix (the new `ORDER BY` happens to put the right row
  first, so `updated[0]`-style assertions still hold) while testing
  nothing true about the new behavior, and it also **will not compile**
  after Task 4's `delete_member` signature change (it calls
  `delete_member(&db, "gpt-4o", "p1")` with the old 3-arg signature).
  Update it to:
  - assert that upserting the same `(pool_id, provider_id, model_override)`
    twice **does** update in place (`updated.len()` unchanged, `priority`
    reflects the second call)
  - add a **new** case: upserting the same `provider_id` into the same
    pool with a **different** `model_override` inserts a second row
    (`updated.len() == 2`)
  - update its `delete_member` call for Task 4's new signature (coordinate
    with Task 4 Step 1 — this file needs both changes together)
- [ ] **Step 9: Run to verify it passes:** `cargo test --offline --lib
  pools`, `cargo test --offline --lib core::state`, `cargo test --offline
  --lib onboarding`, `cargo test --offline --lib providers::queries`.

---

### Task 3: Runtime cooldown/circuit-breaker keying — **the critical fix**

Without this task, Tasks 1/2/4/5 ship a schema/API/UI that *lets* you
build multi-model pools whose failover behavior is broken: same-provider
siblings can't fail over to each other (a cooldown on one blocks the
loop from trying the next), and a `NonRetryable` error on one model
permanently misconfigures every sibling model sharing that provider id.
See design doc's "Runtime cooldown/circuit-breaker keying" section for
full rationale — this task is that section's fix.

**Files:**
- Modify: `src/proxy/flow.rs` (every `state.runtime.entry(provider.id...)`
  / `state.runtime.get(...)` site in the failover loop — ~20 sites per the
  review's grep)
- Modify: `src/core/runtime.rs` (if a helper for building/parsing the
  composite key belongs here — keep `ProviderRuntimeState` itself
  unchanged, this is purely about what string keys `AppState.runtime`)
- Modify: `src/providers/routes.rs` (status display around line 530, the
  `reset_to_healthy()` calls around lines 163 and 247)
- Modify: `src/providers/oauth_routes.rs` (the `reset_to_healthy()` call
  around line 157)
- Modify: `src/providers/refresh_task.rs` (background refresh loop's
  runtime-state interaction)

- [ ] **Step 1:** Add a small key-building helper, e.g. in
  `src/core/runtime.rs`:
  ```rust
  /// AppState.runtime's map key. Widened from bare `provider_id` so a
  /// failure on one (provider, model) pair - e.g. one pool member's
  /// model_override - doesn't cool down or misconfigure its siblings
  /// sharing the same provider/credential. `\u{1f}` (unit separator) can't
  /// appear in either half (both are validated path-id-like strings /
  /// model names), so this is unambiguous and doesn't need escaping.
  pub fn runtime_key(provider_id: &str, model: &str) -> String {
      format!("{provider_id}\u{1f}{model}")
  }
  ```
  Keep `ProviderRuntimeState` (the struct itself, `is_available`,
  `record_retryable`, `mark_misconfigured`, `reset_to_healthy`) unchanged
  — only the map's *key* changes, not its value type.
- [ ] **Step 2: `src/proxy/flow.rs`** — replace every
  `state.runtime.entry(provider.id.clone())` /
  `state.runtime.get(&provider.id)` in the failover loop with
  `state.runtime.entry(runtime_key(&provider.id, effective_model))` (the
  loop already has `effective_model` in scope from
  `selection.providers: Vec<(&Provider, String)>` — confirm the exact
  local variable name at each site, it may be named `model` or bound via
  destructuring). Do this for **all** sites: availability check, both
  `record_retryable` branches (initial attempt + retry-after-refresh
  branches), both `mark_misconfigured` branches, and any others the
  `grep -n "state.runtime" src/proxy/flow.rs` from the review turns up —
  re-run that grep at the start of this step and check every result off,
  don't rely on the count from the review being exhaustive by the time
  you implement.
- [ ] **Step 3: `src/providers/routes.rs`'s status display** (~line 530,
  `let entry = s.runtime.get(&id);`) — a provider's admin-facing status is
  no longer a single lookup; iterate `s.runtime` for keys with prefix
  `` &format!("{id}\u{1f}") `` and report the worst status across them
  (e.g. `Misconfigured` if any key is, else the shortest remaining
  cooldown if any, else healthy) so the provider list still shows one
  status per provider row.
- [ ] **Step 4: `src/providers/routes.rs`'s two `reset_to_healthy()` call
  sites** (~lines 163, 247, both after a credential update) — change from
  `s.runtime.get_mut(&id)` to iterating/removing every entry whose key has
  the `{id}\u{1f}` prefix and calling `reset_to_healthy()` on each (or
  simply `retain`-ing them out of the map and letting them lazily
  recreate as healthy on next use — pick whichever this file's existing
  style favors, they're equivalent since `or_default()` on a fresh entry
  is healthy).
- [ ] **Step 5: `src/providers/oauth_routes.rs`'s `reset_to_healthy()`
  call** (~line 157, after an OAuth completion) — same prefix-based fix
  as Step 4.
- [ ] **Step 6: `src/providers/refresh_task.rs`** — check how the
  background refresh loop reads/writes `s.runtime` for that provider id
  and apply the same prefix-aware treatment; if it only reads
  `needs_refresh`/credential state and never touches `s.runtime` directly,
  confirm that and note "no change needed" rather than skipping silently.
- [ ] **Step 7: Add tests** in `tests/proxy_failover.rs` (or wherever Task
  6 below lands its new tests — coordinate, this can be one test file):
  - one provider, two pool members with different `model_override`s, the
    first member's mock returns 500 — assert the request **fails over**
    to the second member's model within the same request (this is the
    test that a first draft of this plan wrote incorrectly assuming it
    would already work; it only passes once this task is done).
  - one provider, two pool members with different `model_override`s, the
    first member's mock returns a `NonRetryable`-classified error (e.g.
    400) — assert a *subsequent* request still reaches the *second*
    member successfully (proves the first model's `Misconfigured` flag
    didn't take down its sibling).
- [ ] **Step 8: Run to verify it passes:** `cargo test --offline --test
  proxy_failover` (outside any sandbox — binds a real `TcpListener`, per
  `CLAUDE.md`'s standing constraint) and `cargo test --offline --lib
  proxy::flow` / `--lib providers`.

---

### Task 4: Allow deleting one specific `(provider, model)` member

**Files:**
- Modify: `src/pools/queries.rs` (`delete_member`, and — coordinate with
  Task 2 Step 8 — its call site in the `pool_and_member_crud` test)
- Modify: `src/pools/routes.rs` (`delete_member` handler — add optional
  query param)
- Modify: `tests/proxy_failover.rs` (its `add_pool_member` test helper
  currently has no `model_override` parameter — extend it, since Task 3
  Step 7 and Task 6 both need to add same-provider-different-model
  members through it)

- [ ] **Step 1:** Change `queries::delete_member`'s signature to accept an
  `Option<&str>` model filter:
  ```rust
  pub async fn delete_member(
      db: &SqlitePool,
      pool_id: &str,
      provider_id: &str,
      model: Option<&str>,
  ) -> Result<(), AppError> {
      let n = match model {
          Some(m) => {
              sqlx::query(
                  "DELETE FROM pool_members WHERE pool_id = ? AND provider_id = ? AND COALESCE(model_override, '') = ?",
              )
              .bind(pool_id).bind(provider_id).bind(m)
              .execute(db).await?.rows_affected()
          }
          None => {
              sqlx::query("DELETE FROM pool_members WHERE pool_id = ? AND provider_id = ?")
                  .bind(pool_id).bind(provider_id)
                  .execute(db).await?.rows_affected()
          }
      };
      if n == 0 { Err(AppError::NotFound) } else { Ok(()) }
  }
  ```
  Preserve today's default (no `model` → delete every member row for that
  provider in the pool) — deliberate, see design doc's "Out of scope"
  note on why this default is kept despite being a sharp edge for direct
  API callers.
- [ ] **Step 2:** Update every existing caller of the old 3-arg
  `delete_member` (the route handler below, and Task 2 Step 8's test fix)
  to pass the new 4th argument — this is a compile-breaking signature
  change, not additive at the Rust level even though it's additive at the
  HTTP API level.
- [ ] **Step 3:** In `src/pools/routes.rs`, extract an optional `model`
  query param on the `DELETE /admin/pools/:id/members/:provider_id` route
  via a `#[derive(Deserialize)] struct DeleteMemberQuery { model:
  Option<String> }` with axum's `Query<...>` extractor — confirmed during
  review (live-tested against `serde_urlencoded 0.7`, what axum 0.7's
  `Query` uses under the hood) that this correctly distinguishes: param
  absent entirely → `None`; `?model=` (present, empty) → `Some("")`;
  `?model=a%2Fb` → `Some("a/b")`. No `#[serde(default)]` needed. Prefer
  the typed struct over a raw `HashMap<String,String>` — the map form
  silently accepts a duplicate `?model=&model=` query string, the typed
  struct correctly rejects it. **Document the empty-string subtlety** in
  a route comment: `?model=` (present, empty) means "the member with
  `model_override IS NULL`"; the param being absent entirely means
  "delete every member for this provider, regardless of model" (today's
  behavior). Pass `req.model.as_deref()` through to
  `queries::delete_member`.
- [ ] **Step 4: Add tests** in `tests/admin_pools.rs`:
  - deleting without `?model=` when a provider has 2 members (different
    models) removes both — regression guard for the "keep today's
    behavior" claim.
  - deleting with `?model=<x>` removes only that one, leaving the other
    member intact.
  - deleting with `?model=` (empty) removes only the no-override member.
- [ ] **Step 5: Run to verify it passes:** `cargo test --offline --test
  admin_pools`.

---

### Task 5: Admin UI — composite member keys + model picker

**Files:**
- Modify: `frontend/src/pages/Pools.tsx`
- Modify: `frontend/src/pages/Pools.reorder.test.tsx` — review found this
  file's existing assertions will break and named the exact spots: mock
  handlers matching `"/admin/pools/openai/members/b"` and
  `".../members/team%2Fgpt"` (no `?model=` suffix) and delete-request
  assertions checking those same URLs — all need updating once the delete
  button always appends `?model=`.

- [ ] **Step 1:** Add a small helper, e.g. `memberKey(m: PoolMember):
  string => \`${m.provider_id}\u0000${m.model_override ?? ""}\``, and
  replace **every** place currently keying by bare `member.provider_id` —
  review found six, not the four a first draft caught:
  - `<SortableContext items={members.map(memberKey)}>`
  - `<li key={memberKey(member)}>`
  - `moveMember`/`onDragEnd`'s `findIndex(member => memberKey(member) ===
    id)` (dnd item ids become `memberKey(member)` too)
  - **`memberDeleteKey`** (currently `` `member:${pool.id}:${member.provider_id}` ``,
    compared against `pendingDelete` state) — must include the model, or
    two same-provider members both trigger the same "Remove from pool?"
    confirm bar from either one's click, and confirming either could
    delete the wrong/ambiguous target.
  - **`removeMember`'s optimistic local-state update** (currently
    `.filter((m) => m.provider_id !== providerId)`) — must filter on the
    composite key, or removing one same-provider member optimistically
    removes *both* from the UI until the next refetch corrects it.
- [ ] **Step 2:** Update `removeMember(pool, providerId)` to
  `removeMember(pool, providerId, modelOverride)`, and have it call
  `DELETE .../members/:providerId?model=<encoded modelOverride, or empty
  string if undefined>` — the per-row delete button always targets one
  specific row, so it should always send `model` (even as `?model=` for a
  no-override row), never the "delete all for this provider" bulk form.
- [ ] **Step 3:** Wire the "add member" model field to `GET
  /admin/providers/:id/list-models` as a `<select>` (or `<input
  list="...">` datalist) instead of free text, reusing the existing
  `ModelFetchState` plumbing that already fetches this list for
  validation.
- [ ] **Step 4:** After a successful `addMember`, clear only
  `modelOverride` in the draft state, not `providerId` — so adding several
  models from the same provider is provider-pick-once, model-pick-per-add.
- [ ] **Step 5: Add a regression test** — a pool with two members of the
  *same* `provider_id` and different `model_override`s reorders correctly
  and deletes the *correct* row (not both, not the wrong one). **Note:**
  review found no `useSortable`/`useDraggable` anywhere in
  `frontend/src/` — `DndContext`/`SortableContext` are currently inert
  (nothing is draggable, `onDragEnd` never fires); the only working
  reorder path today is the ↑/↓ buttons (`moveMember`). Write this test
  using the buttons, not a simulated drag — a drag-based test would pass
  for the wrong reason (nothing listens for the drag) or not compile
  against working interactions at all. Separately worth a one-line note
  in `BACKLOG.md` that the dnd wiring is dead code, but fixing that is out
  of scope here.
- [ ] **Step 6: Update `Pools.reorder.test.tsx`'s existing mock-URL and
  assertion strings** for the new always-`?model=` delete calls (the
  specific lines named above).
- [ ] **Step 7: Run to verify it passes:** this project's frontend test
  command in `frontend/`.

---

### Task 6: End-to-end verification over real HTTP

Mirrors the round-robin plan's Task 7 — prove the fix over a real bound
`TcpListener`, not just unit-level query behavior. Coordinate with Task 3
Step 7 (some of these may already exist from that task; don't duplicate).

**Files:**
- Modify: `tests/proxy_failover.rs` (extend `add_pool_member` to accept an
  optional `model_override`, per Task 4's note)

- [ ] **Step 1:** One provider, `PUT` into a pool twice with different
  `model_override`s and adjacent priorities — assert both `PUT`s succeed
  (no unique-violation error) and `GET /admin/pools/:id/members` lists
  both rows.
- [ ] **Step 2:** Round-robin test: one provider, two `model_override`s,
  `strategy: round_robin` — assert consecutive `/v1/chat/completions`
  calls alternate between the two models (mirrors
  `round_robin_alternates_across_two_healthy_providers` but with one
  provider instead of two — this is the fix's actual target scenario).
- [ ] **Step 3:** Failover-tail test: same provider/two models, first
  model's mock returns 500 — assert the request fails over to the second
  model **within the same request** (this only passes with Task 3 done;
  if written before Task 3 lands, it correctly fails, which is the point).
- [ ] **Step 4: Run to verify it passes:** `cargo test --offline --test
  proxy_failover` — outside any sandbox (binds a real `TcpListener`, per
  `CLAUDE.md`'s standing constraint).

---

### Task 7: Full-suite regression pass + manual real-provider smoke test

- [ ] **Step 1:** `cargo test --offline` (full workspace) — must stay
  green, including the pre-existing round-robin suite from
  `feature/pool-round-robin` (this branch is based on it).
- [ ] **Step 2:** Manual smoke test against a **throwaway instance**
  (fresh `ROUTER_SQLITE_PATH`, non-8080 `ROUTER_LISTEN_ADDR`, real
  Command Code credentials pulled from `~/.commandcode/auth.json` via
  `POST /admin/providers/:id/commandcode/key` with an empty body) —
  recreate the exact scenario from this session: **one** `command-code`
  provider row, added to one round-robin pool three times with
  `model_override` = `deepseek/deepseek-v4-flash`,
  `deepseek/deepseek-v4-pro`, `MiniMaxAI/MiniMax-M3`. Confirm real
  `/v1/chat/completions` calls rotate across all three models using the
  single provider row, and — the part that couldn't have worked before
  Task 3 — deliberately break one model temporarily (e.g. a wrong model
  name causing a 400) and confirm the *other two* keep serving instead of
  the whole pool going dark.
- [ ] **Step 3:** Tear down the throwaway instance/processes; confirm the
  real production gateway (port 8080, `E:\1router\1router.db`) was never
  touched, per the standing project convention.
- [ ] **Step 4:** Update `.superpowers/sdd/progress.md` marking this plan
  complete, and confirm `docs/superpowers/BACKLOG.md` has entries for
  Option C (credential/provider split) and the `import_config`
  strategy/sticky_limit drop bug found during design review.

---

## Notes for whoever executes this

- This branch (`feature/pool-member-model-identity`) is based on
  `feature/pool-round-robin`, not `master` — round-robin is fully
  implemented there (all 7 tasks done, tests green) but **not yet merged
  to master**. Decide/confirm merge order with the user before opening a
  PR: either merge round-robin to master first and rebase this branch, or
  merge both together.
- Every manual verification step in this plan must run against a
  throwaway db/port, never `E:\1router\1router.db` / port 8080 — see
  Global Constraints.
- Task 3 (runtime keying) is not a nice-to-have polish pass — it's load
  -bearing for the feature to work as advertised. If time-constrained,
  everything else can slip before this can.
