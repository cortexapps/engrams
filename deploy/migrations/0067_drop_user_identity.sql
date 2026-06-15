-- ADR 0039 Task 31: drop the human-identity tables and the sessions owner column.
-- user_tokens has FK → users; web_sessions has FK → users.  Drop dependents first.
DROP TABLE IF EXISTS user_tokens;
DROP TABLE IF EXISTS web_sessions;
DROP TABLE IF EXISTS users;

ALTER TABLE sessions DROP COLUMN IF EXISTS user_id;
