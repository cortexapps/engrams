-- ADR 0007 / Phase 5: memory-manifest persistence on snapshots.
-- Mirrors 0018's `disk_manifest_*` shape for the FC memory side.
-- The UFFD handler reads `memory_manifest_id` + `memory_manifest_version`
-- at restore time to resolve per-fault chunk lookups against the
-- chunk store (and against the canonical-base memory mmap).
--
-- VZ snapshots stay `memory_manifest IS NULL` indefinitely —
-- macOS Virtualization.framework's memory snapshot is broken
-- upstream for arm64 guests (ADR 0003), so the chunked memory
-- path is FC-only. The disk side already lights up on VZ.
--
-- Both columns nullable + check-constrained "both or neither"
-- (so half-populated rows can't happen).

ALTER TABLE snapshots
    ADD COLUMN IF NOT EXISTS memory_manifest_id      UUID,
    ADD COLUMN IF NOT EXISTS memory_manifest_version BIGINT;

ALTER TABLE snapshots
    ADD CONSTRAINT snapshots_memory_manifest_both_or_neither
    CHECK (
        (memory_manifest_id IS NOT NULL AND memory_manifest_version IS NOT NULL)
        OR
        (memory_manifest_id IS NULL     AND memory_manifest_version IS NULL)
    );

-- GC scan: "which memory manifest_ids are live?" pairs with the
-- existing `list_live_disk_manifest_ids` query. Partial index so
-- only chunked-memory rows take index space (VZ rows are skipped).
CREATE INDEX IF NOT EXISTS idx_snapshots_memory_manifest
    ON snapshots (memory_manifest_id)
    WHERE memory_manifest_id IS NOT NULL;
