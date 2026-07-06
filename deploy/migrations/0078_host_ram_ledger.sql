-- Issue #540: the host RAM ledger's attribution columns.
--
-- `allocatable_mib` (migration 0058, ADR 0046) already carries the derived
-- "what's free to place on" figure; these three columns are the ATTRIBUTION
-- it never had — where the RAM actually went. Sampled once per heartbeat
-- tick by `engram-host-agent`'s `ram_ledger::RamLedgerSnapshot` alongside
-- the existing util_* columns (migration 0058), surfaced on the fleet view
-- (api/hosts.rs HostView / fleet.proto) for operator visibility.
--
-- `base_shm_pending_mib` (the in-flight prewarm charge) deliberately has NO
-- column here: it's transient host-local state already folded into the
-- persisted `allocatable_mib`, visible directly via the host's own
-- `/metrics` (`engram_host_ram_ledger_mib{category="base_shm_pending"}`) —
-- not durable multi-replica-consistent state PG needs to hold.
--
-- Additive + serde-defaulted: pre-0078 hosts (and non-Linux dev backends)
-- report 0 for all three, same posture as every other util_* column.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_base_shm_mib BIGINT NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_parked_pss_mib BIGINT NOT NULL DEFAULT 0;
ALTER TABLE hosts ADD COLUMN IF NOT EXISTS util_running_pss_mib BIGINT NOT NULL DEFAULT 0;
