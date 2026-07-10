-- Session titles: the latest harness-suggested (LLM-generated) title for a
-- session. Written in real time by the coordinator's harness event sink when a
-- TitleSuggested event arrives; read by the orchestrator (via the Session
-- proto) to build a task's display title. A self-descriptive operational fact,
-- never user attribution/preference (the sticky user rename lives on the
-- orchestrator's task row).

ALTER TABLE sessions
    ADD COLUMN suggested_title TEXT;
