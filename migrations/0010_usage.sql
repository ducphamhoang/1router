-- Token usage per request, for per-person / per-model reporting and cost
-- estimates (GET /admin/usage).
--
-- `usage_log` is written when a response body finishes (token counts only
-- exist at the end of a stream), so it is a separate table from request_log,
-- which is written when the response starts. `user_id` is the same bare
-- string as request_log.user_id (a users.id, 'admin', 'cf:<email|client id>',
-- or NULL for anonymous). `input_tokens` is the TOTAL input including cached
-- tokens; `cached_tokens` is the cached subset of it.
CREATE TABLE usage_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    TEXT NOT NULL,
    user_id       TEXT,
    pool_id       TEXT,
    provider_id   TEXT,
    model         TEXT NOT NULL,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_usage_log_user ON usage_log(user_id, created_at);
CREATE INDEX idx_usage_log_time ON usage_log(created_at);

-- Maps a credential id to the person it belongs to, so a service token
-- ('cf:<client id>.access') or a users.id rolls up under one name in reports.
-- 'cf:<email>' callers are already a person and need no row.
CREATE TABLE usage_people (
    user_id TEXT PRIMARY KEY,
    person  TEXT NOT NULL
);

-- USD per million tokens, keyed by upstream model name. A request's model is
-- priced by the longest `model` here that is a prefix of it (so one row for
-- `gpt-5.6` covers `gpt-5.6-sol`). No rows are seeded: a model with no price
-- is reported with a null cost rather than a guessed one.
CREATE TABLE model_prices (
    model            TEXT PRIMARY KEY,
    input_per_mtok   REAL NOT NULL,
    output_per_mtok  REAL NOT NULL,
    cached_per_mtok  REAL
);
