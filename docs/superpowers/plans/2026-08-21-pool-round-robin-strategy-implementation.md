# Pool Round-Robin Strategy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a `Pool` an opt-in `round_robin` selection strategy alongside
the existing static-`priority` fallback, so normal-path traffic can be spread
across a pool's members instead of always hammering the lowest-priority one
— while composing for free with the existing failover loop in
`proxy/flow.rs` (round-robin only changes *which member is tried first*; the
rest of the ordered list stays as the fallback tail).

**Design reference:** Ported from 9router's combo-rotation design
(`open-sse/services/combo.js::getRotatedModels` / `rotateModelsFromIndex` /
`resetComboRotation`), cloned to
`.../scratchpad/9router` for this investigation. Key ideas kept:
in-memory rotation cursor keyed by pool id, advance-by-one on each
selection, and an optional **sticky limit** (N consecutive selections on
the same member before rotating) so a strategy switch doesn't thrash a
provider connection on every single request. 9router's simpler
`connectionProxy.js::pickProxyPoolId` (pure sequential cycling, no
stickiness) is the fallback shape when `sticky_limit` is 1 (the default).

**Architecture:** `pools::select::select()` stays the single source of
provider ordering (`proxy/flow.rs`'s failover loop just walks
`selection.providers` front-to-back and never reorders it itself — see Task
4). Round-robin is implemented as an alternate ordering step inside
`select()`: after the existing priority sort, if `pool.strategy ==
RoundRobin`, rotate the sorted `Vec` so a per-pool cursor's member becomes
the head, then advance the cursor (mod the member count, so a resized
member list can never leave a stale cursor out of range — same trick as
9router's `currentIndex = state.index % models.length`). The cursor lives
in a new `AppState` field (a `DashMap<pool_id, RotationState>`), not inside
`ConfigSnapshot` — the snapshot is wholesale-replaced by `ArcSwap` on every
config reload, so it's the wrong place for a live, request-incrementing
counter.

**Tech Stack:** Existing deps only — `dashmap`, `sqlx`, `serde`. No new
crate.

## Global Constraints

- Package is `router`, binary is `1router`. Build/test with `cargo build
  --offline` / `cargo test --offline`.
- **No new Cargo dependency** — the rotation cursor is a plain
  `dashmap::DashMap`, same crate `AppState.runtime`/`refresh_locks` already
  use.
- axum is pinned to **0.7**: the new `PUT /admin/pools/:id` route uses
  `:id`, never `{id}`.
- **`AppState` has six real struct-literal construction sites**, not just
  `src/main.rs` — this is the same shape of trap as the
  `ConnectInfo<SocketAddr>` gotcha already logged in `CLAUDE.md`. Adding a
  new field means updating **all** of:
  - `src/main.rs`
  - `tests/common/mod.rs` (`spawn_app`, used by most integration tests)
  - `tests/admin_pools.rs` (`test_state()`)
  - `tests/admin_settings.rs`
  - `tests/health_stats.rs`
  - `tests/open_access.rs` (`state_for()`)
  Missing one fails to compile (good — it's a hard error, not a silent
  gap), but budget time for it; don't stop after `main.rs` + one test file.
- `sqlx`'s SQLite `ALTER TABLE ADD COLUMN` requires either a constant
  default or a nullable column (already the pattern used by
  `migrations/0003_pool_member_model_override.sql`). `strategy` gets `NOT
  NULL DEFAULT 'priority'`; `sticky_limit` is nullable with no default
  (`NULL` means "use 1").
- Model the new enum exactly like `ProviderKind` (not `WireFormat`, which
  uses `lowercase` for single-word variants): `#[sqlx(rename_all =
  "snake_case")]` + `#[serde(rename_all = "snake_case")]`, since
  `RoundRobin` needs to become `round_robin`.
- Out of scope (v1): per-request health-weighted rotation, a `random`
  strategy, cross-instance-shared rotation state (it's process-local
  in-memory, resets on restart — matching every one of 9router's three
  implementations), rotation for the direct `<provider_id>/<model>`
  addressing path (`select_direct_provider` always returns a single-entry
  vec; strategy is meaningless there).

---

### Task 1: `PoolStrategy` enum, `Pool` fields, migration

Add the enum and the two new `Pool` columns. No behavior change yet — every
existing pool defaults to `Priority`, so `select()`'s output is unchanged
until Task 3 adds the branch.

**Files:**
- Add: `migrations/0004_pool_strategy.sql`
- Modify: `src/core/model.rs` (`Pool` struct ~line 35; new enum near
  `ProviderKind`)
- Test: `cargo test --offline --lib core::model`

**Interfaces:**
- Produces: `PoolStrategy::{Priority, RoundRobin}` (DB text `priority` /
  `round_robin`), `Pool.strategy: PoolStrategy`, `Pool.sticky_limit:
  Option<i64>`. Tasks 2, 3, 5, 6 all depend on these.

- [ ] **Step 1: Write the failing test**

  In `src/core/model.rs`'s `mod tests`, add
  `pool_strategy_serializes_as_snake_case` asserting
  `serde_json::to_string(&PoolStrategy::RoundRobin) == "\"round_robin\""`
  and the round-trip back, plus `PoolStrategy::Priority ==
  "\"priority\""`.

- [ ] **Step 2: Run to verify it fails**

  Run: `cargo test --offline --lib core::model`
  Expected: FAIL to compile — no type named `PoolStrategy`.

- [ ] **Step 3: Implement**

  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
  #[serde(rename_all = "snake_case")]
  #[sqlx(rename_all = "snake_case")]
  pub enum PoolStrategy {
      Priority,
      RoundRobin,
  }
  ```

  Add to `Pool`:
  ```rust
  pub strategy: PoolStrategy,
  pub sticky_limit: Option<i64>,
  ```

  `migrations/0004_pool_strategy.sql`:
  ```sql
  -- Opt-in round-robin selection alongside the existing static-priority
  -- fallback. sticky_limit is nullable ("use 1" = rotate every request);
  -- meaningful only when strategy = 'round_robin'.
  ALTER TABLE pools ADD COLUMN strategy TEXT NOT NULL DEFAULT 'priority';
  ALTER TABLE pools ADD COLUMN sticky_limit INTEGER;
  ```

- [ ] **Step 4: Run to verify it passes**

  Run: `cargo test --offline --lib core::model`

---

### Task 2: Rotation cursor storage on `AppState`

Add the in-memory rotation map and thread it through every `AppState`
construction site (see Global Constraints — six sites total).

**Files:**
- Modify: `src/core/state.rs` (new `PoolRotationMap` type + `AppState`
  field, near `RefreshLocks`)
- Modify: `src/main.rs`, `tests/common/mod.rs`, `tests/admin_pools.rs`,
  `tests/admin_settings.rs`, `tests/health_stats.rs`,
  `tests/open_access.rs`
- Test: `cargo build --offline && cargo test --offline` (compile-only
  gate for this task — no new assertions yet)

**Interfaces:**
- Consumes: nothing.
- Produces: `AppState.pool_rotation: PoolRotationMap` where
  `PoolRotationMap = Arc<DashMap<String, RotationState>>` and
  ```rust
  #[derive(Clone, Copy, Debug, Default)]
  pub struct RotationState {
      pub index: usize,
      pub consecutive_uses: u32,
  }
  ```
  Task 3 reads/writes it inside `select()`; Task 5 clears an entry on pool
  deletion.

- [ ] **Step 1: Implement**

  Add `RotationState` + `PoolRotationMap` to `src/core/state.rs`, add
  `pub pool_rotation: PoolRotationMap` to `AppState`, and
  `pool_rotation: Arc::new(dashmap::DashMap::new()),` at every
  construction site listed above.

- [ ] **Step 2: Run to verify it passes**

  Run: `cargo build --offline` then `cargo test --offline` — this task
  is purely additive plumbing; a green compile across every listed site
  *is* the pass condition.

---

### Task 3: Round-robin ordering in `pools::select::select()`

The core logic. Changes `select()`'s signature to accept the rotation map,
and adds the rotate-and-advance step for `RoundRobin` pools.

**Files:**
- Modify: `src/pools/select.rs` (`select()` ~line 36; new private
  `rotate_from_cursor` helper; `mod tests` — extend `snap()`/`prov()`
  helpers, add new cases)
- Modify: `src/proxy/flow.rs` (the one call site — pass
  `&state.pool_rotation` through; see Task 4, but the signature change
  lands here)
- Test: `cargo test --offline --lib pools::select`

**Interfaces:**
- Consumes: `AppState.pool_rotation` (Task 2), `Pool.strategy` /
  `Pool.sticky_limit` (Task 1).
- Produces: `select(snapshot, pool_id, wire, rotation)` (new 4th param).
  `Selection.providers` ordering now varies call-to-call for
  round-robin pools. `proxy/flow.rs`'s failover loop (Task 4) needs no
  change beyond passing the new argument — it already just walks the
  vec front-to-back.

- [ ] **Step 1: Write the failing tests**

  In `src/pools/select.rs::tests`, extend `snap()` to take a
  `PoolStrategy` (or add a sibling `snap_round_robin()`), and add:
  - `round_robin_rotates_start_index_on_each_call` — two providers `a`,
    `b`, `sticky_limit: None` (defaults to 1); first `select()` call
    returns `[a, b]` (priority order unchanged on the first call, cursor
    starts at 0), second call returns `[b, a]`, third call returns
    `[a, b]` again (wraps).
  - `round_robin_respects_sticky_limit` — `sticky_limit: Some(3)`;
    three consecutive calls all return `[a, b]` (same head), the fourth
    returns `[b, a]`.
  - `round_robin_cursor_wraps_when_member_removed` — seed the rotation
    map with an out-of-range index (simulating a member having been
    deleted since the cursor last advanced) and assert `select()` still
    returns a valid full-length vec (via `% members.len()`), not a panic
    or truncated list.
  - `priority_strategy_never_rotates` — same two-provider pool but
    `strategy: Priority`; assert `select()` returns `[a, b]` on every
    call regardless of prior invocations (regression guard: default
    behavior for every pre-existing pool must stay byte-for-byte
    identical).
  - Update every existing test in this file to pass a fresh
    `Arc::new(DashMap::new())` (or a shared test helper
    `empty_rotation()`) as the new 4th argument.

- [ ] **Step 2: Run to verify it fails**

  Run: `cargo test --offline --lib pools::select`
  Expected: FAIL to compile — `select()` takes 3 arguments, 4 supplied
  (or vice versa, depending on write order); once compiling, the new
  round-robin assertions fail against the unchanged priority-only
  implementation.

- [ ] **Step 3: Implement**

  ```rust
  pub fn select<'a>(
      snapshot: &'a ConfigSnapshot,
      pool_id: &str,
      wire: WireFormat,
      rotation: &PoolRotationMap,
  ) -> Option<Selection<'a>> {
      if let Some(pwm) = snapshot.pools.iter().find(|p| p.pool.id == pool_id) {
          if pwm.pool.wire_format != wire {
              return None;
          }

          let mut members = pwm.members.clone();
          members.sort_by_key(|m| m.priority);

          if pwm.pool.strategy == PoolStrategy::RoundRobin && members.len() > 1 {
              members = rotate_from_cursor(&pwm.pool, members, rotation);
          }

          let providers = members /* ...unchanged mapping to (provider, model)... */;
          return Some(Selection { pool: Some(&pwm.pool), providers });
      }
      select_direct_provider(snapshot, pool_id, wire)
  }
  ```

  `rotate_from_cursor` mirrors `combo.js`'s `getRotatedModels` +
  `rotateModelsFromIndex`: read the entry (default `RotationState`),
  `let head = state.index % members.len();`, rotate `members` so index
  `head` is first (shift-and-rotate, same as the JS `shift()`/`push()`
  loop — or just `members.rotate_left(head)`, which is the same
  operation in one call), then decide whether to advance:
  `sticky_limit` normalizes like 9router's `normalizeStickyLimit` (any
  non-positive or absent value clamps to `1`); if
  `consecutive_uses + 1 >= sticky_limit`, store `{ index: (head + 1) %
  len, consecutive_uses: 0 }`, else `{ index: head, consecutive_uses:
  consecutive_uses + 1 }`.

- [ ] **Step 4: Run to verify it passes**

  Run: `cargo test --offline --lib pools::select`

---

### Task 4: Wire the rotation map into `proxy/flow.rs`

One-line call-site change — confirm the failover loop needs nothing else.

**Files:**
- Modify: `src/proxy/flow.rs` (~line 53, the `select(&snapshot,
  &pool_id, wire)` call)
- Test: `cargo test --offline --test proxy_failover`

**Interfaces:**
- Consumes: `select()`'s new signature (Task 3), `state.pool_rotation`
  (Task 2).
- Produces: no new public interface — this is strictly plumbing so the
  existing failover loop keeps working unchanged.

- [ ] **Step 1: Implement**

  Change the call to `select(&snapshot, &pool_id, wire,
  &state.pool_rotation)`. Do **not** touch the `for (provider,
  effective_model) in &selection.providers` loop itself — it already
  iterates in whatever order `select()` hands back.

- [ ] **Step 2: Run to verify it passes**

  Run: `cargo test --offline --test proxy_failover` (pre-existing tests
  must stay green — they exercise `Priority`-strategy pools only, so
  this is a pure regression check for this task).

---

### Task 5: Admin API — create/update strategy, reset rotation on mutation

Extend `CreatePool`, add a `PUT /admin/pools/:id` endpoint (pools
currently have no update path at all — only create/delete), and clear a
pool's rotation entry whenever its strategy changes or the pool is
deleted (a stale cursor left over from a deleted pool is harmless — it
just never gets read again — but clearing it avoids unbounded growth of
the map over a long-running process).

**Files:**
- Modify: `src/pools/routes.rs` (`CreatePool` DTO ~line 33; `create()`
  ~line 39; new `PoolPatch` DTO + `update()` handler; `delete_pool()`
  ~line 54; route table ~line 15)
- Modify: `src/pools/queries.rs` (`insert_pool`; new
  `update_pool_strategy`)
- Test: `cargo test --offline --test admin_pools`

**Interfaces:**
- Consumes: `PoolStrategy`, `Pool.sticky_limit` (Task 1),
  `AppState.pool_rotation` (Task 2).
- Produces: `POST /admin/pools` accepts optional `strategy` (defaults
  `priority`) and `sticky_limit`; new `PUT /admin/pools/:id` accepts a
  partial `{ strategy, sticky_limit }` patch and calls
  `reload_snapshot()` same as every other mutation in this file, plus
  `state.pool_rotation.remove(&id)`. `DELETE /admin/pools/:id` also
  removes the rotation entry.

- [ ] **Step 1: Write the failing tests**

  In `tests/admin_pools.rs`:
  - `create_pool_defaults_to_priority_strategy` — POST without
    `strategy` in the body; fetched pool has `strategy: "priority"`.
  - `create_pool_accepts_round_robin_strategy` — POST with `{"id":
    ..., "wire_format": ..., "strategy": "round_robin",
    "sticky_limit": 3}`; fetched pool reflects both fields.
  - `put_pool_updates_strategy` — create a `priority` pool, `PUT
    /admin/pools/:id` with `{"strategy": "round_robin"}`, assert the
    change persists and a subsequent `GET` reflects it.
  - `put_pool_rejects_unknown_id` — 404 for a nonexistent pool id.

- [ ] **Step 2: Run to verify it fails**

  Run: `cargo test --offline --test admin_pools`
  Expected: FAIL — `CreatePool` has no `strategy` field yet; no `PUT
  /admin/pools/:id` route exists (404 on a route that should 200).

- [ ] **Step 3: Implement**

  `CreatePool` gains `#[serde(default)] strategy: PoolStrategy` (needs
  `Default` on the enum, or a manual `#[serde(default =
  "...")]` function — `Priority` as `Default`) and `#[serde(default)]
  sticky_limit: Option<i64>`. `insert_pool` adds the two columns to its
  `INSERT`. New `queries::update_pool_strategy(db, id, strategy,
  sticky_limit)` does an `UPDATE pools SET strategy = ?, sticky_limit =
  ? WHERE id = ?`, returning `AppError::NotFound` on 0 rows affected
  (mirrors `delete_pool`'s existing 0-rows-affected check). New route
  handler calls it, then `reload_snapshot(&s).await?` and
  `s.pool_rotation.remove(&id);`. Add `.route("/:id",
  axum::routing::put(update))` to the router (axum 0.7 syntax — not
  `{id}`). `delete_pool()` handler adds `state.pool_rotation.remove(&id);`
  alongside its existing `queries::delete_pool` call.

- [ ] **Step 4: Run to verify it passes**

  Run: `cargo test --offline --test admin_pools`

---

### Task 6: Admin UI — strategy control on the Pools page

Add the create-form dropdown and an edit control in the pool detail
dialog, following the existing `wire_format` dropdown's shape exactly
(there's no prior "edit an existing pool" affordance to extend — this is
new UI surface).

**Files:**
- Modify: `frontend/src/pages/Pools.tsx` (`Pool` type ~line 14; create
  form ~line 634; `createPool()` ~line 161; `renderDetail()` header
  ~line 373; new `updateStrategy()` handler calling `PUT
  /admin/pools/:id`)
- Modify: `frontend/src/pages/Pools.reorder.test.tsx`
- Test: `cd frontend && npm test -- Pools.reorder`

**Interfaces:**
- Consumes: the admin API from Task 5 (`strategy`, `sticky_limit` on
  `Pool`; `PUT /admin/pools/:id`).
- Produces: no new consumers — this is the leaf of the chain.

- [ ] **Step 1: Write the failing tests**

  In `Pools.reorder.test.tsx`:
  - `create form defaults strategy to Priority (Fallback) and submits no strategy override needed`
    — asserts the default dropdown value round-trips.
  - `create form can select Round Robin and sets a sticky limit` —
    select `round_robin`, type a sticky-limit value, submit, assert the
    POST body includes both.
  - `pool detail shows a strategy editor that PUTs on change` — open
    an existing pool's detail dialog, change the strategy dropdown,
    assert a `PUT /admin/pools/:id` fires with the new value.
  - Extend the priority-reorder-related copy assertions (the page-intro
    text at ~line 605) only if that copy needs to branch per strategy;
    otherwise leave it and note in a comment that the reorder UI still
    matters for round-robin pools (it sets the pool's fixed *tie-order*,
    not the live rotation cursor).

- [ ] **Step 2: Run to verify it fails**

  Run: `cd frontend && npm test -- Pools.reorder`
  Expected: FAIL — no strategy `<select>` exists yet in either the
  create form or the detail dialog.

- [ ] **Step 3: Implement**

  Add `strategy: string; sticky_limit?: number | null` to the `Pool`
  type. Add `strategy`/`stickyLimit` state to the create form, a
  `<select>` mirroring the `wireFormat` one (`STRATEGY_OPTIONS =
  [{value: "priority", label: "Priority (fallback)"}, {value:
  "round_robin", label: "Round Robin (rotate)"}]`, same shape as
  9router's `STRATEGY_OPTIONS` in `combos/page.js`), and a sticky-limit
  number input shown only when `strategy === "round_robin"`. Include
  both in the `createPool()` POST body. In `renderDetail()`, add a
  small strategy editor near the header badge that calls a new
  `updateStrategy(poolId, patch)` doing `PUT /admin/pools/:id` then
  refetching the pool list.

- [ ] **Step 4: Run to verify it passes**

  Run: `cd frontend && npm test -- Pools.reorder`

---

### Task 7: End-to-end round-robin behavior over real HTTP

Prove the whole chain — DB-backed pool config through to which upstream
actually receives consecutive requests — using the existing wiremock
scaffolding.

**Files:**
- Modify: `tests/proxy_failover.rs` (uses existing `create_pool`,
  `add_provider`, `add_pool_member` helpers at the top of the file)
- Test: `cargo test --offline --test proxy_failover`

**Interfaces:**
- Consumes: the full stack from Tasks 1-5.
- Produces: no new interface — this is a pure verification task.

- [ ] **Step 1: Write the failing tests**

  - `round_robin_alternates_across_two_healthy_providers` — two
    wiremock servers behind one `round_robin`-strategy, `sticky_limit:
    1` pool; fire two sequential requests; assert server A got the
    first and server B got the second (or vice versa — assert they
    differ and each got exactly one).
  - `round_robin_respects_sticky_limit_across_requests` —
    `sticky_limit: 2`; fire three requests; assert the first two hit
    the same provider and the third hits the other.
  - `round_robin_still_fails_over_within_one_request` — the
    round-robin-selected head provider 500s; assert the request still
    succeeds via the next provider in the rotated list (proves Task 3's
    "rotation only changes the head, not the failover tail" design
    claim).

- [ ] **Step 2: Run to verify it fails**

  Run: `cargo test --offline --test proxy_failover`
  Expected: FAIL — helper functions likely need a `strategy`/
  `sticky_limit` parameter added (check `create_pool`'s signature at
  the top of the file first; extend it rather than adding a parallel
  helper).

- [ ] **Step 3: Implement**

  Extend the `create_pool` test helper to accept strategy/sticky_limit
  (or add `create_pool_with_strategy`), wire it into the three new
  tests.

- [ ] **Step 4: Run to verify it passes**

  Run: `cargo test --offline --test proxy_failover`
  Note: this test binds real `TcpListener`s via `spawn_app` — per
  `CLAUDE.md`'s documented Codex-sandbox limitation, if this task is
  dispatched into a Codex worktree it will report BLOCKED there
  regardless of correctness; re-run it yourself outside the sandbox
  before considering the task done.

---

## Suggested Execution Order

Tasks 1 → 2 → 3 → 4 are a strict chain (schema → storage → logic → call
site). Task 5 (admin API) can start as soon as Task 1 lands (it only
needs the DTO shape), but its rotation-reset code needs Task 2. Task 6
(frontend) only needs Task 5's API shape agreed, not its full
implementation — the two can run in parallel worktrees. Task 7 needs
everything and runs last.

## After Implementation

Run the full suite once end-to-end: `cargo build --offline && cargo test
--offline` (excluding the sandbox caveat on Task 7's new tests), plus
`cd frontend && npm test`. Update `CLAUDE.md`'s workstream list with a
one-line pointer to this plan once merged, following the existing
pattern for the other completed workstreams.
