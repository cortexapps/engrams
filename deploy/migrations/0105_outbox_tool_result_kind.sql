-- ADR 0089 P1: the generic tool protocol adds a third outbox kind —
-- 'tool_result' rows carry {"tool_call_id": ..., "result_json": ...} down
-- to the harness as HarnessCommand::ToolResult. 0084's CHECK predates it
-- and is checksum-immutable, so widen the constraint here.
ALTER TABLE session_outbox
    DROP CONSTRAINT session_outbox_kind_check;
ALTER TABLE session_outbox
    ADD CONSTRAINT session_outbox_kind_check
    CHECK (kind IN ('prompt', 'answer', 'tool_result'));
