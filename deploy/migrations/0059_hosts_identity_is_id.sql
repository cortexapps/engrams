-- Host identity is `id` (stable: persisted in the node's work_dir, or
-- derived from the K8s node name — ADR 0044 K2). `hostname` is the K8s
-- pod name and CHANGES on every fleet roll, so it cannot carry identity:
-- a successor pod re-registering the same host id under its new pod name
-- missed the legacy ON CONFLICT (hostname) arm and hit hosts_pkey with a
-- duplicate-key 500, looping every 30 s and leaving register-carried
-- fields (host_addr!) permanently stale. Registration now upserts
-- ON CONFLICT (id); hostname is just a mutable label.
ALTER TABLE hosts DROP CONSTRAINT IF EXISTS hosts_hostname_key;
