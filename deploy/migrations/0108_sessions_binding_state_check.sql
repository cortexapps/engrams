-- #896 / ADR 0090 addendum: per-state binding legality, enforced at the
-- storage tier. States that must not own a sandbox — pending/queued
-- (pre-bind) and the terminals (the caller destroyed or abandoned the
-- VM; an unbound terminal row is what authorizes the host-side
-- ownership-oracle reap) — now carry a CHECK. The transition layer
-- (BindingDisposition, both stores) enforces the richer contract
-- including the bound-transient states (evacuating/idle-residue/
-- host_lost); this constraint is the belt-and-suspenders floor that
-- also polices the fused SQL status writers the enum cannot see
-- (enqueue_session_resume, enqueue_evacuating_session_resume,
-- settle_evicted_session_idle, place_queued_session) and the blind
-- fenced_assign_sandbox writer.
--
-- Deploy notes:
-- * Rolling-deploy window: an OLD pod's un-fused destroy op (terminal
--   flip while still bound, cleared in a separate follow-up write) hits
--   this CHECK and retries until that pod rolls to the fused Detach
--   flip. Loud, self-healing, bounded by the deploy.
-- * A fenced_assign_sandbox(Some(..)) racing a terminal flip now fails
--   loudly with a CHECK violation instead of silently re-binding a
--   terminal row — intended (the blind writer keeps no status guard;
--   the follow-up to add one is tracked in the ADR 0090 addendum).

-- Backfill: clear any residual bindings in the now-forbidden states
-- (pre-fusion destroy flips could leave bound-terminal rows).
UPDATE sessions SET sandbox_id = NULL
 WHERE status IN ('pending', 'queued', 'completed', 'failed', 'dead')
   AND sandbox_id IS NOT NULL;

-- The 0034 both-or-neither conditional-CHECK shape.
ALTER TABLE sessions ADD CONSTRAINT sessions_binding_state_check CHECK (
    status NOT IN ('pending', 'queued', 'completed', 'failed', 'dead')
    OR sandbox_id IS NULL
);
