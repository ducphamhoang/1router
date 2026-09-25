-- Image generation (docs/superpowers/plans/2026-09-25-image-generation-codex-plan.md).
-- A pool serves exactly one modality; chat routes never select an image
-- pool and vice versa. request_log.modality NULL = chat (pre-0009 rows).
ALTER TABLE pools ADD COLUMN modality TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE request_log ADD COLUMN modality TEXT;
ALTER TABLE request_log ADD COLUMN units REAL;
