# Dataset Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Revision note:** this plan was independently verified against the live tree by an Opus review pass after the first draft (see `docs/superpowers/specs/2026-08-27-dataset-logging-design.md`'s revision history for context). That pass found three defects that would have shipped real bugs — a missed export/import path that both breaks existing seed files and silently drops the new settings on round-trip, a frontend reorder handler that wipes every member's override on drag, and a self-contradictory instruction for where the per-request toggle gate is resolved that would have broken the exact failover case the two-layer toggle exists to serve. All three, plus several smaller schema/consistency gaps, are fixed in the task bodies below. Where this plan says something with unusual specificity (exact SQL, exact struct shape, an explicit "not X, Y instead"), that's usually because the review pass caught a wrong or ambiguous version of it — don't relax it back.

**Goal:** Let an operator opt a `Provider` (base) and/or a specific `PoolMember` (override) into capturing every successful exchange's raw request/response bytes as JSONL files on disk (`<dataset_log_dir>/{provider_id}/{date}.jsonl`, `dataset_log_dir` defaulting to a `dataset-logs` directory sibling to the sqlite file), for later fine-tuning/distillation curation. Off by default everywhere.

**Architecture:** Two new nullable/boolean DB columns (`providers.dataset_logging`, `pool_members.dataset_logging_override`) resolved once per successful attempt in `pools::select`; a new `src/telemetry/dataset_log.rs` writer mirroring `request_log.rs`'s bounded-channel-plus-background-task shape but appending JSONL instead of inserting SQL rows; exactly two tap points in `src/proxy/flow.rs::handle_proxy` (request body at the top, a generic `Body`-stream tee wrapped around every success-path `Response` before it's returned, with a drop-guard so a client disconnecting mid-stream still produces a record). Admin API + UI get one checkbox each (provider form, pool-member form). A separate, easy-to-miss write path — config export/import and seed loading — needs its own fix (Task 2) or the feature silently breaks existing installs.

**Tech Stack:** Existing deps only — `sqlx` (sqlite), `serde_json`, `futures`, `bytes`, `tokio`, `chrono`, `uuid` (already a dep, `Cargo.toml`, `features = ["v4"]`), `axum` (`Body::into_data_stream`/`Body::from_stream`). **No new crate.**

## Global Constraints

- Package is `router`, binary is `1router`; import via `use router::...`. Build/test with `cargo build --offline` / `cargo test --offline`.
- **No new Cargo dependency.**
- Migration file: `migrations/0006_dataset_logging.sql`. SQLite has no native boolean type; mirror `request_log.success`/`0004_pool_strategy.sql`'s style and use `BOOLEAN` column affinity (sqlx maps Rust `bool` to/from it transparently). SQLite forbids `ADD COLUMN ... NOT NULL` without a default — `providers.dataset_logging` supplies one (`DEFAULT 0`); `pool_members.dataset_logging_override` is nullable, so no default is needed.
- Every request resolves through `pools::select::select()` into a `Selection` (`src/pools/select.rs`). Its `providers: Vec<(&Provider, String)>` field must grow a third element carrying the resolved per-member override — **not** the raw `PoolMember`, to avoid threading a lifetime-bound reference the caller doesn't otherwise need.
- `select_direct_provider` (`select.rs:121-132`, the `<provider_id>/<model>` syntax) never reads or creates a `PoolMember` row — its resolved override is always `None`, meaning "fall back to `provider.dataset_logging`". This is not a bug to fix; it's documented behavior. `ensure_direct_pools_for_unassigned_providers` (`core/state.rs:136-163`) is a *different* mechanism — it inserts a real `pool_members` row (`model_override = NULL`) for any provider with no explicit membership, so a bare `<provider_id>` call (no slash) *does* go through the pool branch with a real, overridable `PoolMember`. Don't conflate the two.
- **`Provider`/`PoolMember` construction sites are numerous — do not assume only `main.rs`/`tests/common/mod.rs` need touching.** Before starting Task 1, run `grep -rn "Provider {" src tests` and `grep -rn "PoolMember {" src tests` to get the current, authoritative list (it will drift as the codebase changes; don't trust a hardcoded list in this plan). As of this writing that's roughly a dozen `Provider{}` sites (including `src/admin/mod.rs`, `src/onboarding.rs` ×4, `src/pools/select.rs`'s tests, all three provider adapters, `src/providers/queries.rs`, `src/providers/refresh_lock.rs`, `src/providers/routes.rs` ×4) and a similar spread for `PoolMember{}`. Likewise, `AppState { .. }` is constructed in **at least a dozen places**, not two — `src/admin/auth/routes.rs`, `src/app.rs`, `src/auth/middleware.rs`, `src/main.rs`, `src/providers/refresh_lock.rs`, `src/providers/refresh_task.rs`, `src/ui_assets.rs`, and `tests/{admin_pools,admin_settings,common/mod,health_stats,open_access}.rs` at minimum (the codebase's own `oauth_routes.rs` has a comment acknowledging this problem) — and `Config { .. }` similarly. Task 4 adds one field to each; re-run the greps there too rather than trusting this list.
- Both `src/main.rs` and `tests/common/mod.rs` construct `AppState`/`Config` and must be updated for the new fields, but they are not the *only* sites — see above.
- axum is pinned to **0.7**: any new route uses `:id`, never `{id}`.
- Logging must never block the hot path: the dataset-log channel send is `try_send` (drop on full), exactly like `request_log`'s `log()` helper in `flow.rs:28-44`.
- Only successful exchanges are logged. No record for any `ErrorClass::NonRetryable` / unresolved `AuthExpired` / exhausted `Retryable` branch. A response that *starts* successfully but is cut short mid-stream (client disconnect, or a transport error after the first byte) still produces exactly one record, with `complete: false` — see Task 5.
- Two new struct fields (`Provider.dataset_logging`, `PoolMember.dataset_logging_override`) must carry `#[serde(default)]` — `Provider`/`PoolMember` are deserialized directly (not just built via query-layer literals) by `src/admin/mod.rs`'s config export/import and `src/seed.rs`'s seed-file loading, neither of which will have the new keys in any pre-existing file. Without `#[serde(default)]`, every existing exported dump and every operator's `ROUTER_SEED_PATH` file fails to deserialize the moment these fields exist. `Pool.strategy` (`core/model.rs:56`) already had to solve this exact problem — same fix, same reason.
- No redaction, no retention/pruning code, no cross-wire-format normalization, no cap on accumulated output size — all explicitly out of scope per the design spec.
- Scope: the two tap points cover `handle_proxy` (real client `/v1/*` traffic) only. Admin-initiated upstream calls (`providers::routes::{validate_model, validate_model_preview, list_models_preview, fetch_live_models}`, `providers::refresh_task`) are intentionally not logged.
- Design spec: `docs/superpowers/specs/2026-08-27-dataset-logging-design.md` — read it first; this plan does not repeat the rationale, only the mechanics.

---

### Task 1: Schema — `providers.dataset_logging` / `pool_members.dataset_logging_override`

**Files:**
- Create: `migrations/0006_dataset_logging.sql`
- Modify: `src/core/model.rs` (`Provider` struct ~line 22-32; `PoolMember` struct ~line 66-75; `mod tests`)
- Modify: every `Provider {` / `PoolMember {` fresh-literal construction site surfaced by the greps in Global Constraints, to supply the new field (compiler-error-driven — the borrow/type checker will name every site that needs it; there is no shortcut around visiting each one). Sites built via struct-update syntax off an existing value (e.g. `flow.rs:89-92`'s `Provider { upstream_model: ..., ..(*provider).clone() }`) need no change — the new field carries through automatically.
- Modify: `src/pools/queries.rs` (`list_members`'s explicit column SELECT) and `src/core/state.rs` (`load_snapshot`'s pool_members SELECT, `state.rs:110-113`) — add `dataset_logging_override` to both column lists. **This is a deviation from the original task split**: these two SELECTs were originally filed under Task 2, but `sqlx::FromRow`'s named-column matching means an explicit-column `SELECT` that omits a struct field fails at *runtime* (`ColumnNotFound`), not compile time — so `PoolMember` gaining the field breaks `list_members`/`load_snapshot` immediately, before Task 2 ever runs, and several existing tests (`pool_and_member_crud`, `load_snapshot_reads_providers_and_pools`, `unassigned_providers_get_direct_pools`, three in `onboarding::tests`) fail with exactly that error the moment Step 3 lands. Fixing it here, not in Task 2, is what keeps `cargo test --offline --lib` green at every task boundary. Task 2 still owns the INSERT/UPSERT column lists and the `ON CONFLICT` clause — this is only the read side.
- Test: `cargo test --offline --lib` (the full lib suite, not just `core::model` — this task's compile-error-driven fixups touch modules across the crate)

**Interfaces:**
- Produces: `Provider.dataset_logging: bool`, `PoolMember.dataset_logging_override: Option<bool>`. Every later task reads these two fields.

- [ ] **Step 1: Write the failing test**

In `core/model.rs`'s `mod tests`, add `provider_and_pool_member_carry_dataset_logging_fields` — construct a `Provider { ..., dataset_logging: true }` and a `PoolMember { ..., dataset_logging_override: Some(false) }` literal (this will fail to compile until Step 3, which is the point).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib core::model` — expected FAIL to compile: `struct 'Provider' has no field named 'dataset_logging'`.

- [ ] **Step 3: Add the migration and the struct fields**

```sql
-- migrations/0006_dataset_logging.sql
-- Opt-in dataset logging: `providers.dataset_logging` is the base setting
-- (also the only one consulted for <provider_id>/<model> direct addressing,
-- which has no PoolMember row); `pool_members.dataset_logging_override`
-- optionally overrides it for one specific pool membership, same
-- nullable-falls-back-to-provider idiom as `model_override`. See
-- docs/superpowers/specs/2026-08-27-dataset-logging-design.md.
ALTER TABLE providers ADD COLUMN dataset_logging BOOLEAN NOT NULL DEFAULT 0;
ALTER TABLE pool_members ADD COLUMN dataset_logging_override BOOLEAN;
```

Add `#[serde(default)] pub dataset_logging: bool` to `Provider` (after `upstream_model`) and `#[serde(default)] pub dataset_logging_override: Option<bool>` to `PoolMember` (after `model_override`), each with a doc comment pointing at the design spec and noting *why* `#[serde(default)]` is there (export/import + seed compatibility — see Global Constraints).

- [ ] **Step 4: Fix every construction site the compiler names, then run to verify it passes**

Run `cargo build --offline` repeatedly, fixing each `missing field` error as it's reported — do not try to pre-enumerate the list by hand, the compiler is authoritative. For fresh literals in test helpers, `dataset_logging: false` / `dataset_logging_override: None` is almost always the right value unless the specific test is about this feature. For `ProviderPatch`-style exhaustive-field construction (e.g. `providers/queries.rs`'s patch-apply tests), add the field there too — that struct grows its own field in Task 2, so a literal there will need updating twice; it's fine to do both now if convenient.

Then run `cargo test --offline --lib` and fix the *runtime* (not compile-time) `ColumnNotFound("dataset_logging_override")` failures it surfaces by applying the `list_members`/`load_snapshot` SELECT fix described in the Files list above — this is expected at this point, not a sign something else is wrong.

Run: `cargo test --offline --lib` — PASS, full lib suite, not just `core::model`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(dataset-logging): add opt-in provider/pool-member columns

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

(`git add -A` deliberately, not an enumerated file list — this task's Step 4 touches every construction site the compiler names, which can't be known in advance.)

---

### Task 2: Persist and read the new columns — including export/import and seed

Wire the two new fields through **every** existing query/serialization path that touches `providers`/`pool_members`, so later tasks can rely on `Provider.dataset_logging` and `PoolMember.dataset_logging_override` being correct everywhere, not just through the "normal" admin CRUD path.

**Files:**
- Modify: `src/providers/queries.rs` (`insert_provider` ~line 36-60: column list + one bind; `ProviderPatch` ~line 7-18: new `pub dataset_logging: Option<bool>` field; its apply block ~line 68-113; `update_provider`'s SQL ~line 116-127 — see the exact literal SQL below, bind order matters)
- Modify: `src/pools/queries.rs` (`upsert_member`'s INSERT column list + `ON CONFLICT ... DO UPDATE SET` — currently only updates `priority` on conflict, must also update `dataset_logging_override`; note the `list_members` SELECT was already fixed in Task 1, not here)
- Modify: `src/admin/mod.rs` — the `ExportDump`/import path has its **own** hand-rolled INSERT SQL for both `providers` and `pool_members`, entirely separate from `providers::queries`/`pools::queries`. Both `import_config`'s INSERT statements need the two new columns added (bound from the deserialized `Provider`/`PoolMember`, which — thanks to Task 1's `#[serde(default)]` — will be `false`/`None` when importing a pre-existing dump that predates this feature). The export side needs no code change (it serializes the structs directly, and the new fields are already on them after Task 1), but add a round-trip test (Step 1 below) proving it actually carries the values, not just that it doesn't crash.
- Modify: `src/seed.rs` — confirm (with a test, not just reading the code) that a seed file lacking the two new keys still loads, now that `#[serde(default)]` is in place. No production code change expected here unless `seed.rs` has its own INSERT path too (check — if it delegates to `providers::queries`/`pools::queries`, it needs nothing further once those are fixed).
- Test: `cargo test --offline --lib providers::queries`, `--lib pools::queries`, `--lib core::state`, `--offline --test admin_export_import`

**Interfaces:**
- Consumes: Task 1's fields.
- Produces: round-trip persistence Task 3/4/5/6 depend on.

- [ ] **Step 1: Write the failing tests**

- `providers::queries::tests`: `insert_and_update_provider_round_trip_dataset_logging` — insert a provider with `dataset_logging: true`, read it back, assert `true`; `update_provider` with `ProviderPatch { dataset_logging: Some(false), .. }`, read back, assert `false`; a patch with `dataset_logging: None` leaves the existing value unchanged.
- `pools::queries::tests`: extend `pool_and_member_crud` (or add a sibling) — `upsert_member` with `dataset_logging_override: Some(true)`, `list_members` returns it; upserting the *same identity* again with `dataset_logging_override: Some(false)` updates it in place (not a new row) — this exercises the `ON CONFLICT ... DO UPDATE` fix.
- `core::state::tests`: extend `load_snapshot_reads_providers_and_pools` — seed a `pool_members` row with `dataset_logging_override = 1`, assert `snap.pools[0].members[0].dataset_logging_override == Some(true)`.
- `tests/admin_export_import.rs`: **two** new tests, not one:
  1. `export_import_round_trips_dataset_logging_fields` — create a provider with `dataset_logging: true` and a member with `dataset_logging_override: Some(false)`, export, wipe the DB, import, assert both values survived.
  2. `import_accepts_a_dump_missing_the_dataset_logging_keys` — hand-construct a JSON export payload (or reuse a fixture) that has every field *except* `dataset_logging`/`dataset_logging_override`, import it, assert it succeeds with the fields defaulting to `false`/`None` — this is the test that actually proves the `#[serde(default)]` fix works, not just that new exports round-trip.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib providers::queries pools::queries core::state` and `cargo test --offline --test admin_export_import` — FAIL (missing field / SQL binds too few `?` placeholders / import silently drops the values).

- [ ] **Step 3: Implement**

`insert_provider`: append `dataset_logging` to the column list and `.bind(p.dataset_logging)` to the bind chain.

`ProviderPatch` + apply block: add `pub dataset_logging: Option<bool>`; `if let Some(v) = patch.dataset_logging { p.dataset_logging = v; }`.

`update_provider`'s SQL — spell it out literally, since bind-order-vs-SQL-order mistakes here silently write the wrong value into the wrong column:
```rust
sqlx::query(
    "UPDATE providers SET name=?, base_url=?, api_key=?, upstream_model=?, wire_format=?, dataset_logging=?, updated_at=? WHERE id=?",
)
.bind(&p.name).bind(&p.base_url).bind(&p.api_key).bind(&p.upstream_model)
.bind(p.wire_format).bind(p.dataset_logging).bind(p.updated_at).bind(id)
.execute(db).await;
```
(`dataset_logging` inserted *before* `updated_at` in both the column list and the bind chain — matching positions, not "append after" in one and "insert before" in the other.)

`pools::queries`: add `dataset_logging_override` to `list_members`'s column list; in `upsert_member`, add it to the INSERT column list, bind `m.dataset_logging_override`, and extend the conflict clause to `DO UPDATE SET priority = excluded.priority, dataset_logging_override = excluded.dataset_logging_override`.

`core/state.rs::load_snapshot`: add `dataset_logging_override` to the `pool_members` SELECT column list (`state.rs:110-113`).

`admin/mod.rs::import_config`: add `dataset_logging`/`dataset_logging_override` to both hand-rolled INSERT statements' column lists and bind chains, reading from the deserialized `Provider`/`PoolMember` values (which are `false`/`None` for a pre-existing dump, real values for a new one).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib` and `cargo test --offline --test admin_export_import` — full suite PASS.

- [ ] **Step 5: Commit**

```bash
git add src/providers/queries.rs src/pools/queries.rs src/core/state.rs src/admin/mod.rs tests/admin_export_import.rs
git commit -m "feat(dataset-logging): persist and load the new columns, incl. export/import

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: Resolve the effective toggle in `pools::select`

**Files:**
- Modify: `src/pools/select.rs` (`Selection.providers` type ~line 13; the pool branch's `filter_map` ~line 54-64; `select_direct_provider` ~line 121-132; `mod tests`)
- Test: `cargo test --offline --lib pools::select`

**Interfaces:**
- Consumes: Task 2's populated fields.
- Produces: `Selection.providers: Vec<(&Provider, String, Option<bool>)>` (provider, effective model, resolved member override) and `pub fn dataset_logging_enabled(provider: &Provider, member_override: Option<bool>) -> bool`. Task 5 consumes both.

- [ ] **Step 1: Write the failing tests**

- `dataset_logging_enabled_prefers_member_override_over_provider_default` — `(provider.dataset_logging = false, member_override = Some(true))` → `true`; `(true, Some(false))` → `false`; `(true, None)` → `true`.
- `select_carries_the_member_override_for_a_pool_routed_call` — a pool with one member whose `dataset_logging_override == Some(true)`; assert `selection.providers[0].2 == Some(true)`.
- `select_direct_provider_always_yields_no_override` — `<provider_id>/<model>` addressing; assert `selection.providers[0].2 == None`, regardless of the provider's own `dataset_logging` value (that's read separately by the caller via `.0`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib pools::select` — FAIL to compile (tuple arity mismatch).

- [ ] **Step 3: Implement**

Change `Selection.providers` to `Vec<(&'a Provider, String, Option<bool>)>`. In the pool branch's `filter_map` (`select.rs:54-64`), add `m.dataset_logging_override` as the third tuple element. In `select_direct_provider` (`select.rs:121-132`), the tuple becomes `(provider, model.to_string(), None)`. Add:

```rust
/// `member_override` is `PoolMember.dataset_logging_override` for a
/// pool-routed call, or `None` for direct-provider addressing (which has
/// no `PoolMember` row at all) — either way, `None` means "inherit the
/// provider's own setting".
pub fn dataset_logging_enabled(provider: &Provider, member_override: Option<bool>) -> bool {
    member_override.unwrap_or(provider.dataset_logging)
}
```

This is the *only* production call site outside `select.rs` itself that the tuple-arity change affects: `proxy/flow.rs:74`'s for-loop destructuring. That fixup belongs to Task 5 (it needs the rest of Task 5's context to be meaningful); for this task, either leave `flow.rs` non-compiling and note it in the commit message, or do the minimal `for (provider, effective_model, _) in &selection.providers` fixup here so the crate still builds standalone — either is fine, but don't do more than that minimal fixup here, since Task 5 owns the real wiring.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib pools::select` — PASS.

- [ ] **Step 5: Commit**

```bash
git add src/pools/select.rs src/proxy/flow.rs
git commit -m "feat(dataset-logging): resolve provider/member override in pools::select

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: `telemetry::dataset_log` writer + `AppState`/`Config`/`TestApp` wiring

The JSONL writer itself, independent of the proxy hot path (Task 5 wires it in). Testable with zero network. **Sequence this after Task 1 lands** — both edit `src/core/model.rs`, and running them out of order just means resolving a trivial merge conflict instead of none; it's not a functional dependency, but land Task 1 first anyway to avoid the churn.

**Files:**
- Create: `src/telemetry/dataset_log.rs`
- Modify: `src/telemetry/mod.rs` (`pub mod dataset_log;`)
- Modify: `src/core/model.rs` (new `DatasetLogEntry` and `LatencyMs` structs, next to `LogEntry` ~line 97-104)
- Modify: `src/core/config.rs` (a `dataset_log_dir_from_env(sqlite_path: &str) -> PathBuf` helper, mirroring `secret_file_path`'s shape ~line 104-112: `ROUTER_DATASET_LOG_DIR` env override, else `<sqlite_path's parent>/dataset-logs`; add `dataset_log_dir: PathBuf` to `Config`)
- Modify: `src/core/state.rs` (`AppState`: new `pub dataset_log_tx: DatasetLogSender` field ~line 91, new `pub type DatasetLogSender = tokio::sync::mpsc::Sender<DatasetLogEntry>;` ~line 21)
- Modify: every `AppState { .. }` and `Config { .. }` construction site surfaced by the Global Constraints greps — same compiler-error-driven approach as Task 1. `tests/common/mod.rs`'s `TestApp` struct additionally needs `pub dataset_log_dir: PathBuf` (a plain field, not part of `AppState`) so Task 5's integration tests can locate the JSONL files written during a test run — point it at a per-test tempdir, not a fixed path, so parallel test runs don't collide.
- Test: `cargo test --offline --lib telemetry::dataset_log`, `--lib core::config`

**Interfaces:**
- Consumes: nothing from earlier tasks except `Provider`/`PoolMember` existing.
- Produces: `pub fn spawn_writer(dir: PathBuf, buffer: usize) -> DatasetLogSender`, `DatasetLogEntry` (fields per the design spec's record schema: `request_id, timestamp, pool_id: Option<String>, provider_id, model, user_id: Option<String>, wire_format, stream, input_body: String, output_body: String, complete: bool, latency_ms: LatencyMs`), `LatencyMs { ttfb_ms: Option<i64>, total_ms: i64 }` (a nested struct, so it serializes as the nested `"latency_ms": {"ttfb_ms": ..., "total_ms": ...}` object the design spec commits to as the on-disk contract — not two flat top-level keys). Task 5 sends entries into it.

- [ ] **Step 1: Write the failing tests**

`core::config`: `dataset_log_dir_prefers_env_over_sqlite_sibling` (env-var test, needs the existing `static ENV_LOCK: Mutex<()>` pattern) — with `ROUTER_DATASET_LOG_DIR` set, that wins; unset, falls back to `<sqlite parent>/dataset-logs`.

`telemetry::dataset_log`, all `#[tokio::test]`:
1. `writer_appends_one_jsonl_line_per_entry` — spawn into a tempdir with `spawn_writer(dir, 64)`, send two entries for the same `provider_id`, drop the sender, sleep briefly, read `<dir>/{provider_id}/{today's date}.jsonl`, assert two lines, each valid JSON matching the sent entry (including the nested `latency_ms` object).
2. `writer_partitions_by_provider_id` — two entries with different `provider_id`s land in two different subdirectories.
3. `writer_creates_missing_directories` — write into a tempdir that doesn't yet contain the target subpath; assert it's created.
4. `writer_sanitizes_a_hostile_provider_id_instead_of_escaping_the_directory` — an entry with `provider_id: "../../evil"` (or containing `\`/`:`/control chars) does **not** create anything outside `dir`; assert the resulting path is confined under `dir` (e.g. by canonicalizing both and checking `starts_with`).
5. `send_never_blocks_when_the_channel_is_full` — `spawn_writer(dir, 1)`, exercise `try_send` directly against the returned sender with the channel already saturated, assert it returns immediately (`TrySendError::Full`) rather than blocking — this test is about the *sender* contract (Task 5's job to call `try_send`, not `.send().await`), not the writer loop's throughput.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib telemetry::dataset_log core::config` — FAIL to compile.

- [ ] **Step 3: Implement**

`LatencyMs`/`DatasetLogEntry` — plain `#[derive(Clone, Debug, Serialize, Deserialize)]` structs; nesting `LatencyMs` inside `DatasetLogEntry` is what makes "append one JSON line" produce the design's nested-object schema for free (`serde_json::to_string(&entry)` + `"\n"`).

A path-sanitizing helper, used by the writer before ever joining `provider_id` onto a filesystem path:
```rust
/// `provider_id` is admin/operator-controlled and, on the export/import
/// path, not even validated by `core::error::validate_path_id` (which
/// only rejects empty strings and `/`) — never join it onto a filesystem
/// path unsanitized. Keeps `[A-Za-z0-9._-]` only, collapses anything else
/// to `_`, and refuses `.`/`..` outright.
fn sanitize_path_component(raw: &str) -> String {
    if raw.is_empty() || raw == "." || raw == ".." {
        return "_invalid".to_string();
    }
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}
```

`dataset_log.rs::spawn_writer(dir: PathBuf, buffer: usize) -> DatasetLogSender` — same shape as `request_log::spawn_writer` (bounded `mpsc::channel(buffer)`, `tokio::spawn` loop draining `rx.recv()`), but each received entry:
1. `let safe_id = sanitize_path_component(&entry.provider_id);` then `tokio::fs::create_dir_all(dir.join(&safe_id)).await` (best-effort; log a `tracing::warn!` and drop the entry on failure, same as `request_log::flush`'s failure handling — never panic the writer task).
2. Append `serde_json::to_string(&entry)? + "\n"` to `dir.join(&safe_id).join(format!("{}.jsonl", entry.timestamp.format("%Y-%m-%d")))` via `tokio::fs::OpenOptions::new().append(true).create(true)`.

`core/config.rs::dataset_log_dir_from_env`:
```rust
pub fn dataset_log_dir_from_env(sqlite_path: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("ROUTER_DATASET_LOG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    match std::path::Path::new(sqlite_path).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("dataset-logs"),
        _ => PathBuf::from("dataset-logs"),
    }
}
```

`AppState`: add `pub dataset_log_tx: DatasetLogSender`. `main.rs`: `let dataset_log_tx = telemetry::dataset_log::spawn_writer(config.dataset_log_dir.clone(), 4096);` right after the existing `spawn_writer` call; add `dataset_log_tx` to the `AppState` literal. Mirror in `tests/common/mod.rs`, additionally populating the new `TestApp.dataset_log_dir` field with the same tempdir path passed to `spawn_writer`.

Also add `dataset-logs/` to `.gitignore`, matching how `*.db`/`.router_secret` are already ignored.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib` and `cargo test --offline --test startup` (confirms `AppState`/`Config` still construct cleanly end-to-end everywhere) — PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(dataset-logging): JSONL writer, AppState/Config/TestApp wiring

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: Wire the two tap points into `proxy::flow::handle_proxy`

The hot-path change. Both taps are gated on `dataset_logging_enabled(...)`; when `false`, no accumulator is allocated.

**The per-attempt gate, precisely** (this is where the first draft of this plan was self-contradictory — read carefully): `handle_proxy`'s `for (provider, effective_model, member_override) in &selection.providers` loop tries providers in order; the toggle must be evaluated **per attempt, from that attempt's own `(provider, member_override)`**, not once before the loop starts from `selection.providers[0]`. A `started_at = Utc::now()` / `total_start = Instant::now()` pair *can* be captured once, unconditionally, before the loop (cheap, and needed regardless of whether any given attempt ends up logged) — but `request_id` generation and the `dataset_logging_enabled(...)` check happen **inside the loop, at the point an attempt is about to be tried**, using that iteration's own `provider`/`member_override`. Concretely: if provider A (logging off) is tried first and fails over to provider B (logging on) which succeeds, the record must still be written for B's attempt — get this wrong and the exact failover scenario the two-layer toggle exists to handle silently produces nothing.

**Files:**
- Create: `src/proxy/dataset_tee.rs` (the generic `Body`-stream tee)
- Modify: `src/proxy/mod.rs` (`pub(crate) mod dataset_tee;` — internal, not part of the public `proxy` surface)
- Modify: `src/proxy/flow.rs` (for-loop destructuring ~line 74; `total_start`/timing near the top of `handle_proxy` ~line 51-68; the three success-response sites at ~line 133-147, ~line 263-277, ~line 441-455 — note `provider` at these sites is the *shadowed clone* from `flow.rs:89-92`, which preserves `dataset_logging` via its `..(*provider).clone()`, so reading `provider.dataset_logging`/using `member_override` there is safe, just be aware the name means something different before vs. after line 89)
- Test: `cargo test --offline --lib proxy::dataset_tee`, `--offline --test proxy_streaming`, `--test proxy_direct_provider`

**Interfaces:**
- Consumes: Task 3's `Selection.providers` tuple + `dataset_logging_enabled`, Task 4's `DatasetLogEntry`/`AppState.dataset_log_tx`.
- Produces: no new public surface — this is where the feature actually fires.

- [ ] **Step 1: Write the failing tests**

`proxy::dataset_tee`, pure unit tests (`#[tokio::test]`, `futures::stream::iter` as the fake upstream body stream, no network):
1. `tee_forwards_every_chunk_unchanged` — feed a multi-chunk stream, collect the wrapped `Body`'s output, assert it's byte-identical to the input.
2. `tee_fires_the_callback_once_with_the_full_accumulated_bytes_and_complete_true_when_the_stream_ends_cleanly`.
3. `tee_fires_the_callback_with_complete_false_on_a_mid_stream_upstream_error` — a stream that yields two chunks then an `Err` — the callback fires once (with the two chunks' bytes, `complete: false`), and a *second* poll of the wrapped stream after the error returns `None` rather than re-polling the already-errored inner stream.
4. `tee_fires_the_callback_with_complete_false_if_the_body_is_dropped_before_it_ever_ends` — construct the tee, poll it partway (consume one chunk via `.next().await` once) so some bytes are accumulated, then **drop the stream without polling it to completion** (simulating a client disconnect) — assert the callback still fires exactly once, synchronously as part of the drop, with `complete: false` and whatever was accumulated so far. This is the test that proves the drop-guard actually works — it is the single most common truncation case in production (a user hitting stop) and the one most likely to be silently unhandled by a naive `Stream::poll_next`-only implementation.
5. `tee_never_fires_the_callback_twice` — drive a stream to a normal completion, then confirm nothing further invokes the callback (guards against the drop-guard firing again after the explicit in-stream completion already fired it).
6. `tee_handles_a_single_fixed_chunk_body_identically_to_a_streamed_one` — proves non-streaming and streaming responses go through the exact same code path (no special-casing needed at this layer — the same-wire passthrough adapter always returns `Body::from_stream` regardless of what the client asked for, so there is no "already-formed bytes" case to special-case here).

Integration, extending existing files:
7. `tests/proxy_streaming.rs`: `dataset_logging_writes_a_jsonl_record_for_an_enabled_provider_streaming_response` — enable `dataset_logging` on a test provider, make a streaming request through `tests/common::spawn_app` (reading `TestApp.dataset_log_dir`), assert the file contains one line whose `input_body`/`output_body` round-trip the request/response, `stream: true`, `complete: true`.
8. `tests/proxy_direct_provider.rs`: `dataset_logging_uses_the_provider_default_for_direct_addressing` — a provider with `dataset_logging: true` called via `<provider_id>/<model>` (no pool) produces a record with `pool_id: null`; the same provider with `dataset_logging: false` produces none.
9. `dataset_logging_is_off_by_default_and_writes_nothing` (either file).
10. `dataset_logging_fires_for_the_winning_failover_attempt_not_the_first_one_tried` — a pool with member A (`dataset_logging_override: Some(false)`, made to fail — e.g. a bad `base_url`) at priority 1 and member B (`dataset_logging_override: Some(true)`) at priority 2; the request succeeds via B; assert exactly one record is written, for B's `provider_id`. This is the regression test for the contradiction called out above.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib proxy::dataset_tee` — FAIL to compile. Run the integration tests — FAIL (no file written / module doesn't exist).

- [ ] **Step 3: Implement**

`dataset_tee.rs` — a drop-guard is required, not optional, because a plain `futures::stream::unfold` closure only ever runs again if the stream is polled again; a client-dropped body is never polled again, so the callback must live inside a value whose `Drop` impl is the actual guarantee, with the two "reached a real terminal item" cases (`None`/`Err`) explicitly disarming it first so it doesn't double-fire:

```rust
struct FireOnDrop<F: FnOnce(Bytes, bool)> {
    acc: Vec<u8>,
    cb: Option<F>,
}

impl<F: FnOnce(Bytes, bool)> FireOnDrop<F> {
    /// Explicit terminal firing (stream ended or errored) — disarms the
    /// drop guard so it doesn't fire a second time when this value is
    /// later dropped.
    fn fire_now(&mut self, complete: bool) {
        if let Some(cb) = self.cb.take() {
            cb(Bytes::from(std::mem::take(&mut self.acc)), complete);
        }
    }
}

impl<F: FnOnce(Bytes, bool)> Drop for FireOnDrop<F> {
    /// Safety net for "the body was dropped without ever reaching a
    /// terminal stream item" — a client disconnect. If `fire_now` already
    /// ran, `cb` is `None` and this is a no-op.
    fn drop(&mut self) {
        self.fire_now(false);
    }
}

enum TeeState<S, F: FnOnce(Bytes, bool)> {
    Live(S, FireOnDrop<F>),
    Done,
}

/// Wraps `body` so every chunk still reaches the client unchanged, while a
/// full copy is accumulated and handed to `on_complete` exactly once:
/// `complete: true` if the stream ran to its natural end, `false` if the
/// upstream connection errored mid-stream *or* the client disconnected and
/// the body was dropped before ending (the `FireOnDrop` guard's `Drop`
/// impl is what covers that second case — a plain stream-combinator
/// closure only runs again on the next poll, which a dropped body never
/// gets).
pub fn tee(
    body: axum::body::Body,
    on_complete: impl FnOnce(Bytes, bool) + Send + 'static,
) -> axum::body::Body {
    use futures::StreamExt;
    let inner = body.into_data_stream();
    let guard = FireOnDrop { acc: Vec::new(), cb: Some(on_complete) };
    let stream = futures::stream::unfold(TeeState::Live(inner, guard), |state| async move {
        match state {
            TeeState::Live(mut inner, mut guard) => match inner.next().await {
                Some(Ok(chunk)) => {
                    guard.acc.extend_from_slice(&chunk);
                    Some((Ok(chunk), TeeState::Live(inner, guard)))
                }
                Some(Err(e)) => {
                    guard.fire_now(false);
                    Some((Err(e), TeeState::Done))
                }
                None => {
                    guard.fire_now(true);
                    None
                }
            },
            TeeState::Done => None,
        }
    });
    axum::body::Body::from_stream(stream)
}
```

`flow.rs`:
- For-loop header becomes `for (provider, effective_model, member_override) in &selection.providers`.
- `total_start = Instant::now()` unconditionally, once, before the loop.
- At each of the three success sites, before returning: if `select::dataset_logging_enabled(provider, *member_override)`, build a `DatasetLogEntry` with the fields known at that point (`request_id` generated fresh here via `uuid::Uuid::new_v4()`, `pool_id: selection.pool.map(|p| p.id.clone())`, `provider_id: provider.id.clone()`, `model: effective_model.clone()`, `wire_format: wire`, `stream: client_wanted_stream`, `input_body: String::from_utf8_lossy(&body).into_owned()`, `latency_ms.ttfb_ms` from the already-computed `latency`/`lat2` duration), clone `state.dataset_log_tx` and `total_start` into the `on_complete` closure, and replace `resp`/`response`'s body with `dataset_tee::tee(resp.into_body(), move |output_bytes, complete| { let mut entry = entry; entry.output_body = String::from_utf8_lossy(&output_bytes).into_owned(); entry.complete = complete; entry.latency_ms.total_ms = total_start.elapsed().as_millis() as i64; let _ = dataset_log_tx.try_send(entry); })`, reassembling the `Response` from the original parts + the new body. Skip entirely (return the response untouched) when the toggle resolves `false` — no allocation, no rewrap.
- `providers::routes` and any other non-`handle_proxy` caller are unaffected — this is scoped to `flow.rs` only.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib proxy::dataset_tee` and `cargo test --offline --test proxy_streaming --test proxy_direct_provider --test proxy_failover` — PASS. Confirm `proxy_failover` (unrelated to this feature) still passes unchanged — proves the toggle-off path adds no overhead/behavior change.

- [ ] **Step 5: Commit**

```bash
git add src/proxy/dataset_tee.rs src/proxy/mod.rs src/proxy/flow.rs tests/proxy_streaming.rs tests/proxy_direct_provider.rs
git commit -m "feat(dataset-logging): tap input/output in proxy::flow::handle_proxy

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 6: Admin API + UI — one checkbox each, and the reorder-wipe fix

**Files:**
- Modify: `src/providers/routes.rs` (`CreateBody` ~line 105-115: `#[serde(default)] dataset_logging: bool`; `create` handler ~line 121-152: pass it into the `Provider` literal; `patch` already takes `queries::ProviderPatch` directly, no handler change needed; `mask()` ~line 44-68: add `"dataset_logging": p.dataset_logging`)
- Modify: `src/pools/routes.rs` (`PutMember` ~line 114-120: `#[serde(default)] dataset_logging_override: Option<bool>`; `put_member` ~line 122-150: thread it into the `PoolMember` literal and the JSON response — note `list_members` already returns `Json<Vec<PoolMember>>` directly, so `GET /admin/pools/:id/members` picks up the new field automatically once it exists on the struct, no handler change needed there)
- Modify: `frontend/src/pages/Providers.tsx` (`Provider`/`ProviderForm` type ~line 7-18: `dataset_logging?: boolean`; `emptyForm` ~line 34-42: `dataset_logging: false`; a checkbox in the form JSX ~line 469-524, using the existing unused `.checkbox-row` class from `frontend/src/styles.css:607-616`; `saveProvider` ~line 383-418: include `dataset_logging` in both the create and the PATCH-edit body — the edit branch currently only sends a few fields and must be extended)
- Modify: `frontend/src/pages/Pools.tsx` — **two separate changes, not one**:
  1. The add-member path: `PoolMember` type ~line 7-12: `dataset_logging_override?: boolean`; `addMemberDraft` state ~line 127: add a field; a checkbox in the add-member form ~line 579-650; `addMember` ~line 341-367: include it in the PUT body ~line 352-356.
  2. **`recomputeMemberPriorities` (~line 44-50), used by `persistMembers` (~line 237-255) on every drag-reorder / move-up / move-down**: this function currently rebuilds each member from an explicit field whitelist (`provider_id`, `priority`, `model_override`) and PUTs the rebuilt object for *every* member on every reorder. It must add `dataset_logging_override: member.dataset_logging_override` to that rebuilt object, or the very next drag after this feature ships silently resets every member in the pool back to "inherit provider default" — a real, user-visible data-loss bug distinct from the add-member form. Add a regression test to the existing `frontend/src/pages/Pools.reorder.test.tsx` asserting a reorder preserves a pre-set `dataset_logging_override`.
- Test: `cargo test --offline --test admin_providers --test admin_pools`; `npm test` in `frontend/` (vitest), specifically `frontend/src/pages/Providers.form.test.tsx` and `frontend/src/pages/Pools.reorder.test.tsx`

**Interfaces:**
- Consumes: Tasks 1-2 (the DB columns), no proxy-path dependency.
- Produces: operator-facing toggle.

- [ ] **Step 1: Write the failing tests**

`tests/admin_providers.rs`: `create_provider_accepts_dataset_logging`, `patch_provider_updates_dataset_logging`, `get_provider_response_includes_dataset_logging` (via `mask()`).
`tests/admin_pools.rs`: `put_member_accepts_and_returns_dataset_logging_override`, `put_member_upsert_updates_the_override_in_place` (mirrors the existing model_override upsert-in-place test), `list_members_includes_dataset_logging_override`.
`frontend/src/pages/Providers.form.test.tsx`: checkbox renders, checked state round-trips through save (create and edit).
`frontend/src/pages/Pools.reorder.test.tsx`: **the reorder-preserves-override regression test described above** — this is not optional, it's the direct test for the bug this task exists partly to prevent.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --test admin_providers --test admin_pools` — FAIL (unknown field / response missing key). Run the frontend reorder test — FAIL (override silently reset to `undefined`/`false` after reorder).

- [ ] **Step 3: Implement**

Backend: straightforward field threading per the Files list above — no new validation logic (a plain bool needs none; `Option<bool>` needs none either, `serde`'s default `#[serde(default)]` already handles an absent field as `None`/`false`).

Frontend: one `<label className="checkbox-row"><input type="checkbox" checked={form.dataset_logging} onChange={...} /> Log requests/responses for this dataset</label>` in `Providers.tsx`'s form, and an equivalent per-member checkbox in `Pools.tsx`'s add-member form (default unchecked, only sent as `true` when the operator opts in — mirrors `model_override`'s "only sent when non-empty" pattern already in `Pools.tsx`). Separately, the one-line fix to `recomputeMemberPriorities` to carry the field through reorders.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --test admin_providers --test admin_pools` and `npm test` in `frontend/` — PASS. Then build the UI and click through: create a provider, tick the checkbox, save, reload, confirm it's still ticked; add two pool members with different override values, drag-reorder them, confirm both retain their values.

- [ ] **Step 5: Commit**

```bash
git add src/providers/routes.rs src/pools/routes.rs frontend/src/pages/Providers.tsx frontend/src/pages/Pools.tsx frontend/src/pages/Providers.form.test.tsx frontend/src/pages/Pools.reorder.test.tsx tests/admin_providers.rs tests/admin_pools.rs
git commit -m "feat(admin): dataset-logging toggle on providers and pool members

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 7: End-to-end coverage and docs

**Files:**
- Modify: `README.md` (document `ROUTER_DATASET_LOG_DIR`, the two toggles, the on-disk JSONL record shape including `complete`/`latency_ms`, and that captured bodies are raw/unredacted by design)
- Modify: `CLAUDE.md` (doc index: add this plan and its design spec)
- Test: `cargo test --offline` (full suite)

- [ ] **Step 1: Confirm coverage, don't just assume it**

Task 5's test 10 (`dataset_logging_fires_for_the_winning_failover_attempt_not_the_first_one_tried`) and Task 6's `Pools.reorder.test.tsx` addition are the two regression tests this task exists to make sure actually landed — if either was skipped or watered down in its originating task, write it now rather than treating this task as documentation-only.

- [ ] **Step 2: Run the full suite**

Run: `cargo test --offline` — full suite PASS, nothing regressed (in particular `proxy_failover`, `proxy_sse_error_on_200`, `admin_export_import` — the latter must include the two new fields in its round-trip assertions per Task 2, and must still accept a pre-existing dump missing them).

- [ ] **Step 3: Docs**

Update `README.md` and `CLAUDE.md` as scoped above.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline` again post-docs-change (docs shouldn't affect this, but confirm) — PASS.

- [ ] **Step 5: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs(dataset-logging): README + CLAUDE.md updates

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage** — every section of `docs/superpowers/specs/2026-08-27-dataset-logging-design.md` maps to a task:

| Spec section | Task |
|---|---|
| Why not extend `request_log` | Task 4 (separate writer/sink) |
| Only successful exchanges, no redaction, no retention | Task 5 (gating), Global Constraints |
| Toggle: two-layer, mirrors `model_override` | Tasks 1-3 |
| Storage: JSONL partitioned by provider, path sanitization | Task 4 |
| Record schema, incl. `complete` and nested `latency_ms` | Task 4 (`DatasetLogEntry`/`LatencyMs`), Task 5 (population) |
| Tap points: two, not per-adapter; client-disconnect handling | Task 5 |
| Export/import and seed compatibility | Task 2 |
| Writer mirrors `request_log` | Task 4 |
| Out of scope | Global Constraints |

**2. Placeholder scan** — no `TBD`; every column name, file path, and struct field is given literally, sourced from direct reads of the current files and cross-checked by an independent verification pass.

**3. Name consistency across tasks** — `Provider.dataset_logging` / `PoolMember.dataset_logging_override` (Task 1) flow unchanged through Task 2 (persistence + export/import), Task 3 (`dataset_logging_enabled`), Task 5 (tap gating), Task 6 (admin API/UI, incl. the reorder fix). `DatasetLogEntry`/`LatencyMs` are defined in Task 4 and populated in Task 5. `dataset_tee::tee` (with its `FireOnDrop` guard) is defined and unit-tested in Task 5 before its `flow.rs` call site in the same task.

**4. Sequencing** — Task 1 must land first (every later task reads the new fields/columns). Task 2 needs 1. Task 3 needs 1. Task 4 has no *functional* dependency on 1-3 but shares `src/core/model.rs` with Task 1 — land after Task 1 to avoid a merge conflict. Task 5 needs 3 and 4. Task 6 needs 1-2 only (no proxy-path dependency) and can run in parallel with Task 5. Task 7 needs everything.

### Critical Files for Implementation
- E:\1router\1router\src\pools\select.rs
- E:\1router\1router\src\proxy\flow.rs
- E:\1router\1router\src\core\model.rs
- E:\1router\1router\src\core\state.rs
- E:\1router\1router\src\admin\mod.rs (export/import — easy to miss)
- E:\1router\1router\src\telemetry\request_log.rs (template for Task 4)
- E:\1router\1router\src\providers\queries.rs
- E:\1router\1router\src\pools\queries.rs
- E:\1router\1router\src\providers\routes.rs
- E:\1router\1router\src\pools\routes.rs
- E:\1router\1router\frontend\src\pages\Providers.tsx
- E:\1router\1router\frontend\src\pages\Pools.tsx
