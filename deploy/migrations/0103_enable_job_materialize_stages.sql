-- ADR 0088 UI follow-up: the materialize-side twin of `warm_stages` — a
-- JSON array of {name, started_at, ended_at} stage records (same
-- WarmStageRecord shape, so the UI renders one timeline component for
-- both), maintained by the enable scanner's materialize progress
-- consumer from the host's streamed stage frames (pull → flatten →
-- pack → chunk). A stage left open (ended_at null) on a failed/killed
-- materialize stays open — same abandoned-stage semantics as
-- warm_stages. Reset on a retry's fresh `pull` frame.
ALTER TABLE enable_jobs
    ADD COLUMN materialize_stages JSONB NOT NULL DEFAULT '[]'::jsonb;
