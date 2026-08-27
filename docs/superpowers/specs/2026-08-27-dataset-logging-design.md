# Dataset logging — Design

## Goal

Let a 1router operator opt a specific credential (`Provider`) or a specific
pool membership (`PoolMember`) into capturing the raw request/response of
every successful exchange that flows through it, as line-delimited JSON
files on disk — a corpus usable later for fine-tuning/distillation. Off by
default everywhere; nothing is captured unless explicitly enabled.

## Why this shape, not a `request_log`-table extension

`request_log` (`migrations/0001_init.sql:39-49`, `src/telemetry/request_log.rs`)
already exists and already logs every attempt (pool_id, provider_id, status,
latency, success) into SQLite, batched through an mpsc channel so logging
never blocks the hot path (`src/proxy/flow.rs:28-44`). That table backs the
admin stats dashboard (`src/telemetry/stats.rs`) and is deliberately tiny —
one row per attempt, no bodies. Dataset logging is a different consumer with
different requirements (large payloads, opt-in per credential, exported for
offline curation, not queried live), so it gets its own writer and its own
storage, following the *pattern* `request_log.rs` established (bounded
channel, background task, best-effort/drop-on-full) rather than extending
that table.

## What gets captured, and what deliberately doesn't

- **Only successful exchanges.** No record is written for a failed/errored
  attempt (matches `ErrorClass::NonRetryable` / `AuthExpired` /
  `Retryable` branches in `handle_proxy`) — training-pair curation doesn't
  want negative examples mixed in, and it halves the write paths that need
  to reason about partial state.
- **Raw bytes, not a normalized schema.** 1router has no canonical
  internal message representation today — translation between OpenAI and
  Anthropic wire formats is pairwise (`claude_bridge.rs`), and Codex/Command
  Code do their own reshaping (`codex/transform.rs`,
  `commandcode/transform.rs`). Building a unified schema is out of scope
  here; each record stores the client-facing request body and the
  client-facing response body exactly as sent, tagged with `wire_format`,
  and normalization (if ever needed) happens later as an offline curation
  step over the raw corpus.
- **No redaction.** This is opt-in per credential/membership — enabling it
  is the admin asserting "I'm fine capturing what actually flows over
  this," so there's no scrubbing pass to get subtly wrong. (`redact()` in
  `src/telemetry/logging.rs:19` exists but is unrelated tracing-log
  scaffolding, unused today, and out of scope here.)
- **No retention/pruning code.** Files rotate daily; that's it. Same as any
  other log directory — ops handles archival/deletion externally. Dataset
  log entries are larger per-record than typical logs (full bodies), so
  disk usage deserves operational attention, but that's not a v1 code
  requirement.

## Toggle: two-layer, mirroring the existing `model_override` pattern

Every request resolves to a `Selection` in `src/pools/select.rs`, which is
one of two shapes:

1. **Pool-routed** (`select()`'s first branch, `select.rs:42-70`): `model`
   matches a real `Pool` row by id. This is also how *every* provider is
   reachable by its bare id alone — `state::ensure_direct_pools_for_unassigned_providers`
   (`src/core/state.rs:136-163`) auto-creates a synthetic one-member pool
   (`pool.id == provider.id`) for any provider with no explicit pool
   membership, so "call this provider directly by id" still goes through a
   real `PoolMember` row.
2. **Direct-provider-addressed** (`select_direct_provider`,
   `select.rs:121-132`): `<provider_id>/<model>` syntax, splitting on the
   first `/` — used when one provider/credential exposes several selectable
   models (e.g. `command-code/deepseek/deepseek-v4-flash`). `Selection.pool`
   is `None` here and **no `PoolMember` row is read or created** — the
   selection is synthesized inline from the `Provider` alone.

Because case 2 has no `PoolMember` to attach a per-membership setting to, a
pool-only or member-only toggle can't cover it. Because a single `Provider`
(credential) can back several pools with different `model_override`s (the
documented reason `model_override` exists — `core/model.rs:70-74`), a
provider-only toggle can't distinguish "log traffic through pool A" from
"log traffic through pool B" when both share a credential. Two layers,
matching `PoolMember.model_override`'s existing nullable-falls-back-to-provider
idiom exactly:

- **`Provider.dataset_logging: bool`** (`NOT NULL DEFAULT 0`) — the base
  setting. Always consulted for direct-provider-addressed calls (case 2,
  where there is nothing to override it), and used as the fallback for
  pool-routed calls whose member has no override.
- **`PoolMember.dataset_logging_override: Option<bool>`** — optional,
  meaningful only for pool-routed calls (case 1). `Some(v)` wins over the
  provider's setting for that specific membership; `None` inherits it.

Resolution at the point a request succeeds: given the winning `(Provider,
effective_model)` and, when available, the `PoolMember` it came from,

```
enabled = pool_member.and_then(|m| m.dataset_logging_override)
              .unwrap_or(provider.dataset_logging)
```

`select()`'s `Selection.providers` currently discards the `PoolMember` after
extracting `(provider, effective_model)` (`select.rs:54-64`); it must carry
the resolved override through as a third tuple element (or an equivalent
per-entry struct) so `handle_proxy` can do this resolution without a second
DB/snapshot lookup.

## Storage: JSONL, partitioned by provider

`<dataset_log_dir>/{provider_id}/{YYYY-MM-DD}.jsonl`, append-only, one line
per completed exchange, where `dataset_log_dir` defaults to a
`dataset-logs` directory *sibling to the sqlite file* (i.e. same directory
as `1router.db`), overridable via `ROUTER_DATASET_LOG_DIR` — this mirrors
the existing `secret_file_path` convention in `core/config.rs` (the
`.router_secret` sidecar file already lives next to the sqlite path for the
same reason), rather than inventing a new fixed top-level `data/`
component. In the documented Docker setup (`README.md`'s `-v
"$PWD/data:/data" -e ROUTER_SQLITE_PATH=/data/1router.db`), this naturally
lands the logs under the mounted `data/` volume for free, since the sqlite
path itself is already there.

`provider_id` is the only identity guaranteed present in both selection
cases above (`pool_id` is `None` for direct-provider addressing), so it's
the stable partition key; `pool_id` is still recorded as a field inside
each line for workflow-level filtering during curation, when present.

`provider_id` is attacker/operator-controlled (admin API, `1router setup`,
or an imported seed/export file — `validate_path_id` rejects only empty
strings and `/`, and the export/import path in `src/admin/mod.rs` doesn't
even call that) and gets used as a directory-name path component, so the
writer must sanitize it before touching the filesystem — reject or replace
anything outside `[A-Za-z0-9._-]` and reject a component that is exactly
`.` or `..`, rather than trusting it's already safe.

No such non-DB storage directory exists in the repo today — this
introduces the convention. `dataset_log_dir` needs to be created on first
write if absent, mirroring how `persist_secret` in `core/config.rs` already
`create_dir_all`s the sqlite path's parent.

## Record schema

```json
{
  "request_id": "...",
  "timestamp": "2026-08-27T12:00:00Z",
  "pool_id": null,
  "provider_id": "command-code",
  "model": "deepseek-v4-flash",
  "user_id": null,
  "wire_format": "openai",
  "stream": true,
  "input_body": "<raw bytes, as received from the client>",
  "output_body": "<raw bytes, as sent to the client, accumulated so far>",
  "complete": true,
  "latency_ms": { "ttfb": 120, "total": 4300 }
}
```

- `pool_id`: `null` for direct-provider-addressed calls; the real pool id
  otherwise.
- `model`: the effective upstream model actually used for this attempt
  (`selection`'s per-entry effective model — a `PoolMember.model_override`
  or the provider's own `upstream_model`), not necessarily what the client
  requested.
- `user_id`: reserved, always `null` until a future "User credential"
  feature exists. Added now specifically so that feature doesn't require a
  schema/file-format migration later — once it lands, wiring it in is
  threading one more value into the record at the same two tap points
  described below, nothing structural. Whether a user should be able to
  opt out even when their pool/provider has logging on is an explicit
  **open question for that future feature**, not decided here.
- `input_body` / `output_body`: opaque strings holding the exact bytes:
  the client's JSON request body, and everything sent back to the client
  (for a streaming response, the full concatenated SSE byte stream — `data:
  {...}\n\n` blocks and all — not a reassembled/parsed message). No parsing
  happens at write time; a later curation/export step owns turning these
  plus `wire_format` into training pairs.
- `complete`: `false` when the response stream ended before finishing
  cleanly — the client disconnected mid-stream, or the upstream connection
  dropped — in which case `output_body` holds only whatever bytes were
  accumulated up to that point. `true` for every response that reached its
  natural end (including a non-streaming response, which "ends" as soon as
  its one chunk is delivered). Curation should discard `complete: false`
  records by default; they exist as a real-but-truncated exchange, not a
  clean training pair. A client disconnecting mid-stream (the single most
  common truncation in practice — a user hitting stop) must still produce
  a record with `complete: false`, not silence.
- `latency_ms.ttfb` / `.total`: time-to-first-byte (the existing duration
  already computed around `state.http.execute` in `handle_proxy`) and total
  wall-clock duration of the whole exchange, which requires a *new* timer
  started right before the response begins streaming back to the client and
  read when the response ends (stream completion or single-chunk delivery)
  — `total` is not derivable from the upstream-request timer alone, since
  it doesn't cover time spent streaming the reply back.

## Tap points: two, not one-per-adapter

`handle_proxy` (`src/proxy/flow.rs:46`) always ends up with one uniform
`axum::response::Response`, returned from
`adapter.transform_response(upstream, client_wanted_stream)`
(`flow.rs:133-136`), regardless of which `ProviderAdapter` produced it or
how much internal SSE reshaping happened getting there (`HttpAdapter`'s
same-wire passthrough is raw bytes untouched; its cross-wire path and both
Codex and Command Code already parse/reshape SSE chunk-by-chunk before
handing back that same `Response` type). That means logging does **not**
need adapter-specific hooks in four different modules — it needs exactly
two taps, both in `flow.rs`:

1. **Input**: top of `handle_proxy`, `body: Bytes` is already in scope
   before the failover loop starts (`flow.rs:51`). Capture it once,
   correlate by a per-request id generated here.
2. **Output**: wrap the `Response` returned by the winning
   `transform_response` call — the success arm at `flow.rs:133-147` (and
   its retry-after-refresh twin at `flow.rs:263-277`, and the Command Code
   transport-fallback twin at `flow.rs:441-455`; all three are the same
   "success" shape reached by different paths through the failover loop) —
   before returning it to the caller. One tee handles streaming and
   non-streaming identically: it wraps the response body's byte stream
   (via `axum::body::Body::into_data_stream()`/`from_stream()`) with a
   combinator that forwards every chunk to the client untouched *and*
   appends it to an accumulator, firing the write when the stream ends.
   This is not a streaming-only concern — even the same-wire passthrough
   path (`HttpAdapter`, most traffic) always returns
   `Body::from_stream(upstream.bytes_stream())` regardless of whether the
   client asked for `stream: true`, so there is no "already-formed bytes"
   shortcut to take for a non-streaming response; a single-chunk stream
   goes through the exact same tee.

   A stream can end three ways, and the tee must produce a record for all
   three: it runs to completion (`complete: true`); the upstream connection
   errors mid-stream (`complete: false`, whatever was accumulated so far);
   or the **client disconnects** and hyper drops the response body before
   it ever reaches a natural end or an error (also `complete: false`) — this
   third case is the most common truncation in practice (a user hitting
   stop) and is easy to miss because it doesn't surface as an `Err` from
   the stream at all, it surfaces as the stream simply never being polled
   to completion. The tee's implementation needs to guarantee the write
   fires even then (e.g. a drop-guard holding the accumulator and the
   channel sender), not just on the two paths that produce an explicit
   stream event.

Both taps are gated on the resolved `enabled` value from the toggle
resolution above — when `false`, no accumulator is allocated and no
struct is even constructed, so a pool/provider with logging off pays
nothing beyond the one boolean check.

Scope: this covers `handle_proxy`, i.e. real client-facing `/v1/*` traffic,
only. Admin-initiated upstream calls that don't go through `handle_proxy` —
model discovery/validation (`providers::routes::{validate_model,
validate_model_preview, list_models_preview, fetch_live_models}`) and the
background credential-refresh loop (`providers::refresh_task`) — are
deliberately not logged; they aren't real workload traffic and shouldn't
pollute a distillation corpus.

## Writer: mirrors `request_log`'s pattern, different sink

A new `src/telemetry/dataset_log.rs`, structurally parallel to
`src/telemetry/request_log.rs`: `spawn_writer` returns a bounded
`mpsc::Sender<DatasetLogEntry>`; a background task drains it and appends
JSONL lines to the per-provider-per-day file, opening/creating
`data/dataset-logs/{provider_id}/` and rotating to a new day's file as
needed. Send is `try_send` from the hot path (never blocks, drops on a full
channel — logging must never be the reason a request is slow), same
discipline as `log()` in `flow.rs:28-44`. Held on `AppState` as a new field
alongside `log_tx`.

## Export/import and seed compatibility

`Provider`/`PoolMember` aren't only written through
`providers::queries`/`pools::queries` — `src/admin/mod.rs`'s config
export/import (`ExportDump`) and `src/seed.rs` both serialize/deserialize
these structs directly, with their own separate INSERT SQL. Two new struct
fields therefore need `#[serde(default)]` (the same fix `Pool.strategy`
already needed for the same reason), or every pre-existing exported dump
and every operator's `ROUTER_SEED_PATH` file fails to deserialize the
moment the fields are added. Both new columns need to actually round-trip
through that import path's INSERT statements too, or an export→import
cycle silently resets every provider/membership's dataset-logging setting.

## Out of scope (v1)

- Cross-wire-format normalization into a single canonical schema — deferred
  to an offline curation/export step that reads the raw JSONL corpus.
- Redaction/PII scrubbing — this is opt-in raw by design.
- Retention, rotation limits, or automatic pruning of old JSONL files.
- Any behavior tied to the not-yet-built "User credential" feature beyond
  reserving the `user_id: null` field.
- Logging failed/errored exchanges.
- Object-store (S3-compatible) backends — local disk JSONL only; swapping
  the sink later is an internal change behind the same writer interface.
- A cap on accumulated output size. `input_body` is already bounded by
  `ROUTER_MAX_BODY_BYTES` (request body limit); a logged response stream
  has no equivalent cap and accumulates fully in memory for the duration of
  any in-flight logged streaming request. Acceptable for v1 given this is
  opt-in per credential/membership, not default-on.
