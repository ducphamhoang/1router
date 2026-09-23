-- Per-model reasoning-effort capability: `providers.default_reasoning_effort`
-- is the base setting (also the only one consulted for
-- <provider_id>/<model> direct addressing, which has no PoolMember row);
-- `pool_members.reasoning_effort_override` optionally overrides it for one
-- specific pool membership, same nullable-falls-back-to-provider idiom as
-- `model_override` / `dataset_logging_override`.
--
-- Unlike the `dataset_logging` precedent these columns carry CHECK
-- constraints: a hand-edited or imported row with a garbage value would
-- otherwise fail to decode inside `core::state::load_snapshot`, which
-- reloads the *entire* gateway's config - one bad row would then break
-- every provider, not just its own.
ALTER TABLE providers ADD COLUMN default_reasoning_effort TEXT NULL
  CHECK (default_reasoning_effort IN ('low','medium','high') OR default_reasoning_effort IS NULL);
ALTER TABLE pool_members ADD COLUMN reasoning_effort_override TEXT NULL
  CHECK (reasoning_effort_override IN ('low','medium','high') OR reasoning_effort_override IS NULL);
