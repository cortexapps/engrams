-- ADR 0036: async image-enable job state machine. (Migration 0052;
-- 0050/0051 were taken by ADR 0034/0035 while this was in flight.)
--
-- POST /api/enabled-images used to run the whole enable pipeline
-- (registry pull → chunk materialize → capture-VM boot + snapshot)
-- synchronously inside the HTTP handler. A 10 GB image takes minutes
-- to materialize; the external LB kills the request at ~30 s and
-- silently cancels the in-flight enable, which is why operators had
-- to port-forward around the LB. The handler now just records an
-- enable_jobs row and returns 202; the coordinator's enable_scanner
-- drives the pipeline:
--
--     pending → materializing → capturing → ready
--                    └──────────┴────────→ failed (after N attempts)
--
-- Multi-coordinator safety: a scanner claims a job by stamping
-- (claimed_by, claimed_at) via an atomic UPDATE guarded on the lease
-- being free or expired. A crashed pod's claim expires and a peer
-- re-claims; every pipeline step is idempotent (content-addressed
-- materialize skip, digest-keyed capture reuse, upsert), so resume
-- is a re-run that fast-forwards. Progress (chunks_done/chunks_total)
-- doubles as the operator-facing progress bar and the resume
-- high-water mark.
--
-- One ACTIVE job per image_uri: re-POSTing while a job is in flight
-- returns the existing job instead of duplicating work (enforced by
-- the partial unique index). Terminal rows (ready/failed) are kept
-- for audit; a new enable of the same URI inserts a fresh row.

CREATE TABLE enable_jobs (
    id              UUID PRIMARY KEY,
    image_uri       TEXT NOT NULL,
    -- OCI manifest digest observed at POST time. Informational (the
    -- scanner re-pulls at materialize time and proceeds with what
    -- the registry serves then); surfaced in the API for operators.
    manifest_digest TEXT,
    state           TEXT NOT NULL DEFAULT 'pending',
    -- Progress: set when the scanner parses the bootstrap; done is
    -- checkpointed during materialize (also renews the claim).
    chunks_total    INTEGER,
    chunks_done     INTEGER NOT NULL DEFAULT 0,
    -- Pipeline failures bump attempts and leave the job in place for
    -- the next tick; the scanner flips to 'failed' (with error
    -- populated) once attempts exceed its budget.
    attempts        INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    claimed_by      TEXT,
    claimed_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- At most one non-terminal job per image URI (re-POST = resume).
CREATE UNIQUE INDEX enable_jobs_active_uri
    ON enable_jobs (image_uri)
    WHERE state NOT IN ('ready', 'failed');

-- The scanner's sweep predicate.
CREATE INDEX enable_jobs_active
    ON enable_jobs (state)
    WHERE state NOT IN ('ready', 'failed');
