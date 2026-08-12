-- ADR 0116 A-D5: sandbox tombstones — the durable "this VM is disowned,
-- its host must destroy it" fact. Written in the SAME transaction that
-- clears a binding without host-affirmed absence (the dead-host bulk
-- orphan; any failed-destroy-then-unbind site), delivered to the host on
-- its next heartbeat response, and deleted when the host's reported
-- running set no longer contains the sandbox (ack-by-absence). Closes
-- the partition-heals hole: a returning marked-dead host destroys its
-- OWN disowned VMs from an explicit coordinator fact instead of a
-- successor's inference.
CREATE TABLE sandbox_tombstones (
    host_id UUID NOT NULL,
    sandbox_id UUID NOT NULL,
    -- The session the sandbox served when it was disowned; forensics
    -- and idempotent re-writes only, never a lookup key.
    session_id UUID,
    created_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (host_id, sandbox_id)
);
