ALTER TABLE session_managed_interrupted_targets ADD COLUMN event_floor BIGINT NOT NULL DEFAULT 0 CHECK (event_floor >= 0);
