-- ADR 0036 amendment: fleet chunk prestage as a NON-terminal enable-job
-- stage (issue #538, INTERIM — see the ADR's "prestage stage (interim)"
-- section). Only 'ready' and 'failed' are terminal (EnableJobState::
-- is_terminal); 'prestaging' sits between 'capturing' and 'ready':
--   pending → materializing → capturing → prestaging → ready | failed
-- `state` is TEXT (migration 0052), so the new value needs no DDL; the
-- partial unique index enable_jobs_active_uri (WHERE state NOT IN
-- ('ready','failed')) already treats 'prestaging' as active — no index change.

ALTER TABLE enable_jobs
    -- The wire-shape EnabledImageRef (serde JSON) the heartbeat ack
    -- advertises to hosts while this job is prestaging. Stamped by the
    -- scanner after capture; NULL before that / for legacy rows.
    ADD COLUMN prestage_ref   JSONB,
    -- Per-host prestage outcome map, written once at stage end. Audit +
    -- dashboard surface; '{}' until the stage runs.
    ADD COLUMN prestage_hosts JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE hosts
    -- True iff this host-agent runs the image-prefetch supervisor
    -- (chunk_store + chunk_cache configured). The enable prestage stage
    -- waits only on hosts with this bit; Process/dev hosts report false
    -- and are exempt. Folds into the capability vector later
    -- (capability-vector-readiness, #531).
    ADD COLUMN stages_images  BOOLEAN NOT NULL DEFAULT FALSE;
