-- RefreshImage(force_recapture=true): persist the operator's intent across
-- the async enable scanner handoff, so refresh can force a fresh base-snapshot
-- capture instead of taking the content/digest reuse fast path.

ALTER TABLE enable_jobs
    ADD COLUMN force_recapture BOOLEAN NOT NULL DEFAULT FALSE;
