-- ADR 0068: typed capability-vector host readiness + the FC snapshot-version
-- pairing check.
--
-- `hosts.capabilities` mirrors the register/heartbeat-carried
-- `HostCapabilities` JSON (schema/backend/grpc_self_connect/base_shm_tmpfs/
-- uffd_minor_shmem/nbd/bundle_stamp/fc_snapshot_version/wire_version) — the
-- self-verified vector the placement filter gates on, instead of the
-- registered-boolean + later-discovered side-gates it replaces. Default
-- '{}'::jsonb deserializes to `schema: 0` (never reported — the same soft
-- posture `wire_version = 0` gets), so existing rows and a host mid-roll stay
-- placeable until they report a real vector.
--
-- `snapshots.fc_snapshot_version` is the capture-time `firecracker
-- --snapshot-version` of the recording host, when known. NULL is the
-- no-constraint case (pre-migration rows, VZ/Process captures, captures
-- recorded without a known host) — the placement gate only requires an
-- exact match when BOTH the snapshot row and the candidate host report a
-- version.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS capabilities JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS fc_snapshot_version TEXT;
