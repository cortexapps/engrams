-- Track A cleanup: cross-host cold resume retired. Sessions that were
-- in `pending_reassign` (host died, awaiting cross-host reschedule) are
-- now `dead` (snapshot invalidated, only fork is valid). The two states
-- aren't semantically identical — pending_reassign meant "we'll bring
-- you back somewhere," dead means "you're terminal" — but in practice
-- the cross-host rehydrate path is gone, so any pending_reassign row is
-- effectively dead today.
--
-- Engram is now a one-shot agent task runner: sessions live ↔
-- FC-snapshot lifetime. The workspace is the cross-session durability
-- primitive (committed to the checkpoint branch); the conversation log
-- is observable but not replayable.

UPDATE sessions SET status = 'dead' WHERE status = 'pending_reassign';
