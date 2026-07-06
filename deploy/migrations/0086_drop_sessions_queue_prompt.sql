-- ADR 0073 (completion): the create-time initial prompt now rides the durable
-- outbox exactly like every follow-up (enqueued by `send_prompt_core` in
-- `create_session_core` the moment the session row commits). The bespoke
-- `sessions.queue_prompt` column that used to stash it for the never-consumed
-- `ENGRAM_INITIAL_PROMPT` env-var delivery path is retired — the prompt lives
-- in `session_outbox` now, for both the placed and queued dispositions.
ALTER TABLE sessions DROP COLUMN IF EXISTS queue_prompt;
