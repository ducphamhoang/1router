CREATE TABLE providers (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    wire_format   TEXT NOT NULL,
    kind          TEXT NOT NULL DEFAULT 'passthrough',
    base_url      TEXT,
    api_key       TEXT,
    upstream_model TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE TABLE provider_oauth_state (
    provider_id       TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    access_token      TEXT,
    refresh_token     TEXT,
    id_token          TEXT,
    access_expires_at TEXT,
    provider_data     TEXT NOT NULL DEFAULT '{}',
    pkce_verifier     TEXT,
    oauth_state       TEXT,
    updated_at        TEXT NOT NULL
);

CREATE TABLE pools (
    id          TEXT PRIMARY KEY,
    wire_format TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE pool_members (
    pool_id     TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    priority    INTEGER NOT NULL,
    PRIMARY KEY (pool_id, provider_id)
);
CREATE INDEX idx_pool_members_pool ON pool_members(pool_id);

CREATE TABLE request_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    pool_id     TEXT,
    provider_id TEXT,
    status_code INTEGER,
    latency_ms  INTEGER NOT NULL,
    success     BOOLEAN NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX idx_request_log_pool ON request_log(pool_id, created_at);
CREATE INDEX idx_request_log_provider ON request_log(provider_id, created_at);
