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
-- to NULL so the sentinel is unambiguous going forward - the query layer
-- (src/pools/queries.rs, src/admin/mod.rs) filters it out at write time
-- too, so it can't reappear.
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
