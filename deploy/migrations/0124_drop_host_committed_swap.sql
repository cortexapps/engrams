-- Swap dirty-file blocks are included in measured disk use.
ALTER TABLE hosts DROP COLUMN util_committed_swap_mib;
