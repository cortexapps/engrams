-- ADR 0051 Task 31: drop the human-identity tables (the auth/identity store).
-- user_tokens has FK → users; web_sessions has FK → users.  Drop dependents first.
DROP TABLE IF EXISTS user_tokens;
DROP TABLE IF EXISTS web_sessions;
DROP TABLE IF EXISTS users;

-- NOTE: `sessions.user_id` is deliberately KEPT (nullable, 0065). The internal
-- `Session` model still threads it (always NULL post-cutover — attribution moved
-- to the orchestrator task model; the gRPC `Session` contract omits it), and the
-- ADR-0048 queue-create path still binds it. Physically dropping the column +
-- removing the remaining code references is a clean follow-up; dropping it here
-- breaks `enqueue_create` ("column user_id does not exist").
