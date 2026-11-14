-- Boot records retain physical host identity after host deregistration.
ALTER TABLE session_managed_boots DROP CONSTRAINT session_managed_boots_host_id_fkey;
