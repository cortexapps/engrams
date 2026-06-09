-- ADR 0046: host-measured allocatable memory for placement.
--
-- `allocatable_mib` is the memory actually available to place NEW sessions on a
-- host: MemAvailable + Σ guest-resident (PSS), sampled by the host-agent every
-- heartbeat (engram-host-agent `util.rs`). It nets out the host daemon, the OS,
-- kube-system pods, the chunk cache, and the mlock'd base-memfile residency
-- (ADR 0022) — everything resident that isn't a running VM — so the placement
-- transaction subtracts only per-session budgets from it. This replaces the
-- coordinator-side residency-floor estimate: the host measures the baseline
-- accurately and tracks its drift (e.g. chunk-cache growth).
--
-- Additive: pre-0058 hosts (and non-Linux dev backends) report 0, where
-- placement treats the host's capacity as unknown and falls through rather than
-- gating on a bogus 0.

ALTER TABLE hosts ADD COLUMN IF NOT EXISTS allocatable_mib BIGINT NOT NULL DEFAULT 0;
