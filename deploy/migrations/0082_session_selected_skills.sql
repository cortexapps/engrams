-- Issue #535 (create-as-a-plan): persist a queued session's selected skills
-- so the queue scanner's boot re-prepare stops losing them (ADR 0055
-- TODO(P1-D) — the scanner previously booted every queued session with base
-- skills only, since the queue row carried no dynamic-mount selection).
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS selected_skills TEXT[] NOT NULL DEFAULT '{}';

-- Issue #535 (b) boot bundle cache invalidation: NOTIFY every coordinator
-- replica when a host's baked bundle stamp (`current_bundles`, the fleet's
-- skill/harness catalog) actually changes, i.e. on a host roll — NOT on
-- every few-seconds heartbeat UPDATE (which always includes the column in
-- its SET list but rarely changes its value). The WHEN guard is what keeps
-- this quiet in steady state; `AFTER ... OF current_bundles` merely scopes
-- which statements re-evaluate the trigger.
--
-- Two triggers, not one: Postgres statically rejects a `WHEN` clause that
-- references `OLD` on an `INSERT` event (`INSERT trigger's WHEN condition
-- cannot reference OLD values`) — `OLD` doesn't exist yet for a freshly
-- inserted row, and the restriction is enforced at `CREATE TRIGGER` time,
-- not per-row, so a runtime `OLD IS NULL` guard can't route around it. The
-- INSERT leg always fires (a new host's first bundle stamp is a "change"
-- by definition); the UPDATE leg carries the `IS DISTINCT FROM` guard.
CREATE OR REPLACE FUNCTION notify_fleet_catalog_changed() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('fleet_catalog_changed', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS hosts_notify_fleet_catalog_changed ON hosts;
DROP TRIGGER IF EXISTS hosts_notify_fleet_catalog_changed_ins ON hosts;
DROP TRIGGER IF EXISTS hosts_notify_fleet_catalog_changed_upd ON hosts;

CREATE TRIGGER hosts_notify_fleet_catalog_changed_ins
    AFTER INSERT ON hosts
    FOR EACH ROW
    EXECUTE FUNCTION notify_fleet_catalog_changed();

CREATE TRIGGER hosts_notify_fleet_catalog_changed_upd
    AFTER UPDATE OF current_bundles ON hosts
    FOR EACH ROW
    WHEN (OLD.current_bundles IS DISTINCT FROM NEW.current_bundles)
    EXECUTE FUNCTION notify_fleet_catalog_changed();
