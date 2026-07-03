-- Issue #539: instrument + harden the capture-time [warm] hook.
--
-- Today a `[warm]`-hook capture (dev-brain: 10-33 minutes) runs under ONE
-- opaque global timeout; the only surviving diagnostic on a failure is the
-- terse `enable_jobs.error` string "[warm] hook exited with status
-- Some(1)" — the actual in-guest stderr only exists in host-agent logs.
-- `BuildBaseSnapshot` becomes a server-streaming RPC (wire v8) that emits
-- `CaptureProgress` at least every 30s; the coordinator persists each
-- event onto this row (which also renews the enable-job claim lease,
-- replacing the blind renewal ticker in enable_scanner.rs), so a live
-- capture is observable and a failed one leaves its failing stage + last
-- 16 KiB of hook output on the row with zero host-log access.

ALTER TABLE enable_jobs
    ADD COLUMN capture_phase         TEXT,          -- 'boot' | 'warm' | 'snapshot' (NULL outside capture)
    ADD COLUMN warm_stage            TEXT,          -- current/last [warm]-hook stage name
    ADD COLUMN warm_stage_started_at TIMESTAMPTZ,
    ADD COLUMN warm_stages           JSONB,         -- [{name, started_at, ended_at, outcome}]
    ADD COLUMN output_tail           TEXT;          -- rolling last 16 KiB of hook stdout+stderr

COMMENT ON COLUMN enable_jobs.capture_phase IS
    'Live capture_and_record_base_snapshot phase: boot | warm | snapshot. NULL before/after the capturing state.';
COMMENT ON COLUMN enable_jobs.warm_stage IS
    'Current (while capturing) or last-known (on failure) [warm]-hook stage name.';
COMMENT ON COLUMN enable_jobs.warm_stage_started_at IS
    'Wall-clock start of warm_stage, for an operator to eyeball how long the current stage has been running.';
COMMENT ON COLUMN enable_jobs.warm_stages IS
    'JSON array of engram_core::types::capture_progress::WarmStageRecord — the full stage history for this capture attempt.';
COMMENT ON COLUMN enable_jobs.output_tail IS
    'Rolling last 16 KiB (UTF-8-lossy) of the [warm] hook''s combined stdout+stderr, kept on success AND failure.';
