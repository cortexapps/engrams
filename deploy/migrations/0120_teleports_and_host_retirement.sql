CREATE TABLE session_teleports (
  id                UUID PRIMARY KEY,
  session_id        UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  kind              TEXT NOT NULL CHECK (kind IN ('snapshot','live')),
  reason            TEXT NOT NULL CHECK (reason IN ('retire_host','admin_drain','ui')),
  phase             TEXT NOT NULL CHECK (phase IN
    ('admitted','captured','restored','committed','attached','done',
     'rolling_back','aborted','failed')),
  source_host_id    UUID NOT NULL,
  source_sandbox_id UUID NOT NULL,
  -- ADR 0123 divergence: retain historical host IDs after host deletion, as on the source.
  dest_host_id      UUID NOT NULL,
  dest_sandbox_id   UUID,
  pinned_dest       BOOLEAN NOT NULL DEFAULT false,
  mem_budget_mib    BIGINT NOT NULL,
  cpu_budget_vcpus  INT NOT NULL,
  snapshot_id       UUID,
  export_id         TEXT,
  attempts          INT NOT NULL DEFAULT 0,
  error             TEXT,
  created_at        TIMESTAMPTZ NOT NULL,
  updated_at        TIMESTAMPTZ NOT NULL,
  finished_at       TIMESTAMPTZ
);
CREATE UNIQUE INDEX session_teleports_one_open
  ON session_teleports (session_id) WHERE phase NOT IN ('done','aborted','failed');
CREATE INDEX session_teleports_open_by_source
  ON session_teleports (source_host_id) WHERE phase NOT IN ('done','aborted','failed');
CREATE INDEX session_teleports_open_by_dest
  ON session_teleports (dest_host_id) WHERE phase NOT IN ('done','aborted','failed');

ALTER TABLE hosts ADD COLUMN cordon_owner TEXT CHECK (cordon_owner IN ('operator','admin')),
    ADD COLUMN cordon_reason TEXT,
    ADD COLUMN retire_requested_at TIMESTAMPTZ,
    ADD COLUMN retired_at TIMESTAMPTZ;
-- A cordon set before this migration belongs to an admin.
UPDATE hosts SET cordon_owner = 'admin' WHERE cordoned AND cordon_owner IS NULL;
ALTER TABLE sessions ADD COLUMN attached_binding_epoch BIGINT NOT NULL DEFAULT 0;
