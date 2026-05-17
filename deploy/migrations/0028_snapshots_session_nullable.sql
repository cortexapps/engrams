-- ADR 0014 M1.11: snapshots.session_id becomes nullable.
--
-- Prior to M1.11, every `snapshots` row referenced a `sessions` row —
-- snapshots were produced by either the idle-evictor or the M2
-- background uploader, both of which are session-driven. The
-- enabled_images cascade (M1.11) introduces a new producer: at image-
-- enablement time, the coord inserts a `snapshots` row representing
-- the canonical template snapshot the image-builder produced at bake.
-- That snapshot has no session_id — it's a template artifact, not a
-- session capture.
--
-- Drop the NOT NULL so the cascade can insert template snapshots
-- without a sentinel session_id. Existing rows are unaffected
-- (they're all session-bound and stay that way). Session-snapshot
-- producers (idle_evictor, M2 background uploader) still bind
-- session_id; only the template-snapshot path leaves it NULL.

ALTER TABLE snapshots
    ALTER COLUMN session_id DROP NOT NULL;
