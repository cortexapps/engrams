-- ADR 0116 A5: the lost-destroy leftover channel. A sandbox in a host's
-- reported running set that NO session row binds is host-affirmed
-- present + coordinator-confirmed unowned — but a BOOTING VM is
-- legitimately running-but-unbound (its binding is written only after
-- create returns), so entombment requires the sighting to persist
-- across a grace window. This table is that debounce: coordinator
-- replicas are stateless per request, so the first-seen mark lives
-- here. Rows are pruned the moment the sandbox is bound, leaves the
-- running set, or graduates to a tombstone.
CREATE TABLE sandbox_unbound_sightings (
    host_id UUID NOT NULL,
    sandbox_id UUID NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (host_id, sandbox_id)
);
