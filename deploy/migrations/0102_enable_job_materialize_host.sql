-- ADR 0088: durable materialize placement. Stamped (fenced by claimed_by)
-- by materialize_image_on_host immediately after the host pick, BEFORE the
-- streaming RPC starts — so in-flight materialize work is never running but
-- unattributed. Liveness is derived, not stored: a materialize is live on
-- this host iff state = 'materializing' AND claimed_at is fresh (the host's
-- <=30s keepalive frames renew the claim via
-- update_enable_job_materialize_progress). The column is never cleared — it
-- is inert outside state='materializing' and doubles as a "where did the
-- last materialize run" breadcrumb.
ALTER TABLE enable_jobs
    ADD COLUMN materialize_host_id UUID;
