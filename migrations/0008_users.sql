-- Users: named bearer credentials for /v1/* (see
-- docs/superpowers/specs/2026-08-28-user-credentials-design.md). Admin issues
-- one key per user; the gateway validates it on each request and records the
-- user's id in request_log / dataset_log. Upstream provider credentials are
-- unaffected.
--
-- `id` is an admin-chosen slug (same idiom as providers.id / pools.id) so it
-- reads directly in log rows without a join. Only the SHA-256 of the raw key
-- is stored; `key_prefix` (the first few characters of the raw key) exists
-- purely so the admin UI can tell keys apart. `revoked_at` is a soft delete,
-- set once and never cleared.
CREATE TABLE users (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    key_prefix   TEXT NOT NULL,
    key_hash     TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at   TEXT
);

-- Who made the request: a users.id, 'admin' for the shared secret, NULL for
-- anonymous open-access calls. A bare string, not a foreign key, so
-- attribution survives however the users table changes.
ALTER TABLE request_log ADD COLUMN user_id TEXT;
CREATE INDEX idx_request_log_user ON request_log(user_id, created_at);
