-- ADR 0013: stateless dial-on-demand transport. The coord needs to know
-- where to dial each host's gRPC server (replacing the long-lived WS).
-- `host_addr` is a full URL like `http://10.10.0.42:9101` written by
-- the host-agent's POST /api/hosts/register at startup.
--
-- Nullable because pre-0013 rows registered via the WS path don't
-- carry an addr; those hosts are effectively dead until they
-- re-register after their next agent restart. The coord's dispatch
-- path treats `NULL host_addr` as "host not reachable via gRPC."

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS host_addr TEXT;
