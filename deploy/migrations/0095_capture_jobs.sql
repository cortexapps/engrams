-- ADR 0081 phase 1: capture becomes a durable, host-executed,
-- epoch-fenced job row dispatched/reported over the heartbeat,
-- replacing the connection-coupled `BuildBaseSnapshot` RPC stream (a
-- dropped stream today keeps running detached host-side while the
-- coordinator re-drives from scratch, booting a duplicate capture VM
-- with no anti-affinity).
--
-- This migration is purely additive/dormant: the new table and column
-- are created but nothing writes to them yet (P1a — schema + metadata
-- verbs + wire v15 fields only; the executor/scanner rework rides a
-- later commit in the same PR).

CREATE TABLE capture_jobs (
    id                  UUID PRIMARY KEY,
    enable_job_id       UUID NOT NULL REFERENCES enable_jobs(id),
    image_uri           TEXT NOT NULL,
    -- Content-derived ManifestRef of the materialized rootfs (ADR
    -- 0080), rendered as `<manifest_id>:<version>` — the executor
    -- needs no PG round-trip to resolve it.
    disk_manifest       TEXT NOT NULL,
    -- The ADR 0080 ImageConfig (refs only; never resolved secrets —
    -- warm env refs are resolved fresh at claim time).
    image_config        JSONB NOT NULL,
    oci_defaults        JSONB NOT NULL,
    host_id             UUID NOT NULL,
    -- Fencing token; bumped on every reassignment. Every host report
    -- carries (job_id, epoch); every coordinator write is fenced
    -- `WHERE id = $1 AND epoch = $2 AND stage NOT IN ('done','failed')`.
    epoch               BIGINT NOT NULL DEFAULT 1,
    -- assigned -> booting -> warming -> freezing -> done | failed
    -- (cold-base hit path skips booting's cold-boot half; warm-less
    --  images skip warming; freezing includes the write-through flush
    --  — chunks are durable at write per ADR 0078 P1)
    stage               TEXT NOT NULL DEFAULT 'assigned',
    stage_started_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- {"detail": "...", "log_tail": "..."} (units per stage).
    stage_progress      JSONB,
    last_progress_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempts            INTEGER NOT NULL DEFAULT 1,
    -- Terminal classification, written from the host's terminal report.
    retryable           BOOLEAN,
    error               TEXT,
    error_stage         TEXT,
    -- Stamped by the capturing host; NULL for VZ/Process (no FC
    -- SNAPSHOT_VERSION concept there).
    fc_snapshot_version TEXT,
    -- CaptureJobResult on stage='done'.
    result_bincode      BYTEA,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- At most one active (non-terminal) job per enable job — the
-- insert-or-get dedup that makes a coordinator restart mid-capture
-- resume instead of duplicate.
CREATE UNIQUE INDEX capture_jobs_active_enable ON capture_jobs (enable_job_id)
    WHERE stage NOT IN ('done', 'failed');

-- One-capture-per-host anti-affinity (ADR 0081 section C) + the
-- `capture_assignments_for_host` heartbeat-ack read.
CREATE INDEX capture_jobs_active_host ON capture_jobs (host_id)
    WHERE stage NOT IN ('done', 'failed');

-- ADR 0081 section D: reuse telemetry stamped on every terminal
-- enable — `reused_full | reused_cold_base | recaptured:no_cold_base |
-- recaptured:content_changed | recaptured:chunks_missing |
-- recaptured:fc_version_changed`.
ALTER TABLE enable_jobs ADD COLUMN reuse_outcome TEXT;

-- ADR 0081 section B: the cold-base / warm-overlay split. Content key
-- is env-agnostic — `sha256(disk_manifest.content_ref ||
-- canonical(image_config.resources) || fc_snapshot_version ||
-- backend_kind)` — computed by the executor, not by SQL.
CREATE TABLE cold_bases (
    content_key         TEXT PRIMARY KEY,
    snapshot_id         UUID NOT NULL,
    disk_manifest       TEXT NOT NULL,
    memory_manifest     TEXT NOT NULL,
    fc_snapshot_version TEXT NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
