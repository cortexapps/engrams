-- Snapshot swap uses the same reference types as the root disk.
ALTER TABLE snapshots
    ADD COLUMN swap_manifest_id UUID,
    ADD COLUMN swap_manifest_version BIGINT,
    ADD CONSTRAINT snapshots_swap_manifest_both_or_neither
        CHECK ((swap_manifest_id IS NULL) = (swap_manifest_version IS NULL));

-- Positional bincode cannot survive SnapshotMetadata field additions.
-- Cold bases are a cache: discard old payloads and capture each key again.
DELETE FROM cold_bases;
ALTER TABLE cold_bases
    ADD COLUMN snapshot_json JSONB NOT NULL,
    DROP COLUMN snapshot_bincode;

ALTER TABLE capture_jobs ADD COLUMN result_json JSONB;
-- Parents before prestaging have not completed finalization. Prestaging/ready parents
-- already have durable snapshot references and do not need the old result.
-- Give format-change retries a fresh budget; the scanner advances the epoch
-- and chooses a host through redrive_failed_capture_job.
UPDATE capture_jobs AS c
SET stage = 'failed',
    retryable = TRUE,
    attempts = 0,
    error = 'capture result invalidated by JSON storage migration; recapture required',
    error_stage = 'freezing',
    stage_progress = NULL
FROM enable_jobs AS e
WHERE e.id = c.enable_job_id
  AND e.state NOT IN ('prestaging', 'ready')
  AND c.stage = 'done'
  AND c.result_bincode IS NOT NULL;
ALTER TABLE capture_jobs DROP COLUMN result_bincode;
