-- ADR 0015 M3: `HostRegistry` becomes a strict read-through cache
-- over `sessions`. On a cache miss (or after the per-host TTL
-- elapses), the registry calls `MetadataStore::host_for_sandbox`,
-- which selects the row whose `sandbox_id = $1`. Without an index
-- that's a sequential scan over every session row on every cache
-- miss — fine on M1 dev, untenable once production carries 10k+
-- historical session rows.
--
-- Partial index: only rows with a non-null `sandbox_id` are
-- routing-relevant (terminal sessions clear it via the M2
-- `assign_session_sandbox(None)` path). The partial form keeps the
-- index narrow as completed/failed rows accumulate.

CREATE INDEX IF NOT EXISTS idx_sessions_sandbox_id
    ON sessions (sandbox_id)
    WHERE sandbox_id IS NOT NULL;
