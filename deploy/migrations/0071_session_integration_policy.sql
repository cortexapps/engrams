-- ADR 0056 (option B′): the per-session integration policy the orchestrator
-- compiled and shipped on CreateSession, persisted verbatim as its JSON string.
--
-- Persisting it lets the queued re-prepare + a post-eviction resume rebuild the
-- egress injections (resolving each inject's secret_ref host-side) without a
-- round-trip to the orchestrator — the coordinator is the authority once the
-- session exists (ADR 0047). One blob per session (upsert). ON DELETE CASCADE:
-- the policy is meaningless without its session.
CREATE TABLE session_integration_policy (
    session_id  UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    policy_json TEXT NOT NULL
);
