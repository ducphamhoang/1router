-- Per-caller API keys for /v1/*. Admin issues one key per user/client; the
-- gateway validates the key on each request and records which caller made
-- it. Upstream provider credentials are unaffected.
--
-- Only the SHA-256 of the raw key is stored; `key_prefix` (the first few
-- characters of the raw key) exists purely so the admin UI can tell keys
-- apart without ever showing the secret again.
CREATE TABLE client_keys (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    key_prefix   TEXT NOT NULL,
    key_hash     TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at   TEXT
);

-- Who made the request: `caller_key_id` is NULL for the shared admin secret
-- and for anonymous open-access calls; `caller_name` is a snapshot of the
-- key's name at request time ('admin' for the shared secret, NULL for
-- anonymous).
ALTER TABLE request_log ADD COLUMN caller_key_id TEXT;
ALTER TABLE request_log ADD COLUMN caller_name TEXT;
CREATE INDEX idx_request_log_caller ON request_log(caller_name, created_at);
