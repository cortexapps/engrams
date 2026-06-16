-- ADR 0048: the session's declared vCPU budget (image manifest
-- resources.suggested_vcpus), recorded at reserve time beside mem_budget_mib so
-- placement can pack against a host's total_vcpus × overcommit budget.
-- NOT NULL DEFAULT 0: pre-0048 rows (and the no-reservation mock path)
-- read 0, which the picker treats as "no CPU constraint" — the same
-- soft posture as an unmeasured host.
ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS cpu_budget_vcpus INTEGER NOT NULL DEFAULT 0;
