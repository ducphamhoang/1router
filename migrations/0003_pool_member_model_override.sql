-- Lets one Provider (one set of credentials, e.g. a single Codex OAuth
-- login) be reused across multiple pools that each call a different
-- upstream model, instead of needing a separate Provider row (and a
-- separate OAuth login) per model.
ALTER TABLE pool_members ADD COLUMN model_override TEXT;
