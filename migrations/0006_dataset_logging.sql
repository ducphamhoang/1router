-- Opt-in dataset logging: `providers.dataset_logging` is the base setting
-- (also the only one consulted for <provider_id>/<model> direct addressing,
-- which has no PoolMember row); `pool_members.dataset_logging_override`
-- optionally overrides it for one specific pool membership, same
-- nullable-falls-back-to-provider idiom as `model_override`. See
-- docs/superpowers/specs/2026-08-27-dataset-logging-design.md.
ALTER TABLE providers ADD COLUMN dataset_logging BOOLEAN NOT NULL DEFAULT 0;
ALTER TABLE pool_members ADD COLUMN dataset_logging_override BOOLEAN;
