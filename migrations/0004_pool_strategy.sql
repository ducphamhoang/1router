-- Opt-in round-robin selection alongside the existing static-priority
-- fallback. sticky_limit is nullable ("use 1" = rotate every request);
-- meaningful only when strategy = 'round_robin'.
ALTER TABLE pools ADD COLUMN strategy TEXT NOT NULL DEFAULT 'priority';
ALTER TABLE pools ADD COLUMN sticky_limit INTEGER;
