-- A session that an automation's agent spawns is that agent's child, never the
-- workstream's kept session. Earlier builds recorded it with the default role.
UPDATE "automation_session" SET "role" = 'child' WHERE "block_id" = 'spawn_session';
