-- ADR 0112: Σ swap_mib over a host's live sandboxes, reported on the
-- heartbeat. The ephemeral swap backing files are anonymous inodes
-- (unlink-after-attach), so this COMMITTED figure — not a walked one —
-- is what placement subtracts from free disk before the floor test,
-- making swap admission reservation-safe instead of reactive.
ALTER TABLE hosts
    ADD COLUMN util_committed_swap_mib BIGINT NOT NULL DEFAULT 0;
