-- ADR 0079 (issue #543): the durable per-session op log — the lifecycle
-- kernel. Every lifecycle verb (create_boot | resume | evict | deliver |
-- destroy | checkpoint_finalize | teleport) becomes a row here, driven by
-- a single-writer-per-session executor. `sessions.current_epoch` is the
-- fencing epoch: CAS-bumped in the same transaction that claims an op,
-- stamped into every PG session-write (`AND current_epoch = $e`) and every
-- session-scoped host RPC (rejected below the host's persisted high-water).
--
-- Numbered 0092: main high-water at land = 0091 (drop_hosts_local_snapshots).
CREATE TABLE session_ops (
    id              BIGSERIAL PRIMARY KEY,
    session_id      UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    state           TEXT NOT NULL DEFAULT 'queued',
    step            TEXT,
    epoch           BIGINT,
    attempts        INT NOT NULL DEFAULT 0,
    not_before      TIMESTAMPTZ,
    idempotency_key TEXT,
    claimed_by      TEXT,
    claimed_at      TIMESTAMPTZ,
    heartbeat_at    TIMESTAMPTZ,
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ
);

-- One RUNNING op per session, enforced by the database — the repo's
-- PG-leasing-row convention applied at op granularity. A racing second
-- claimer fails the transaction on this index.
CREATE UNIQUE INDEX session_ops_one_running ON session_ops (session_id) WHERE state = 'running';
-- Caller-supplied idempotency: at most one ACTIVE (queued|running) op
-- with a given (session, kind, key). Scoped to the active states on
-- purpose (ADR 0079 review finding #4): a *terminal* keyed row
-- (done|failed|cancelled) must NOT burn the key forever — otherwise a
-- keyed evict that failed terminally (`evict:{last_active_at}`,
-- `evict-descend:{parked_at}`) would make every subsequent scanner/reaper
-- enqueue a no-op Duplicate, wedging the session Evicting with a live /
-- parked VM. A fresh enqueue after a terminal row lands a new row; the
-- ON CONFLICT arbiter in `op_enqueue_tx` names this exact predicate.
CREATE UNIQUE INDEX session_ops_idem ON session_ops (session_id, kind, idempotency_key) WHERE idempotency_key IS NOT NULL AND state IN ('queued', 'running');
-- Head-of-queue scan.
CREATE INDEX session_ops_head ON session_ops (session_id, id) WHERE state = 'queued';
-- The reclaim sweep's input: running ops whose executor stopped stamping.
CREATE INDEX session_ops_running_hb ON session_ops (heartbeat_at) WHERE state = 'running';

ALTER TABLE sessions ADD COLUMN current_epoch BIGINT NOT NULL DEFAULT 0;
