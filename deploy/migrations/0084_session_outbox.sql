-- ADR 0073 / epic #542 phase 2: the coordinator-durable command outbox.
-- Every command down to the guest (prompt, interactive answer) is a row
-- here until the confirming harness event acks it; the host queue is a
-- non-durable relay. SendPrompt = user-echo emit + one INSERT + notify
-- + 202. Redelivery is safe end-to-end: the harness dedups prompts by
-- prompt_id (ADR 0052 seen_prompt_ids) and answers are no-op on
-- duplicate (ADR 0054).
--
-- NOTE: numbered 0084 against the 2026-07-05 high-water mark (0083 =
-- binding_epoch in this same PR). Re-verify at land time.
CREATE TABLE session_outbox (
    prompt_id    TEXT PRIMARY KEY,          -- client-minted; answers use 'answer:<tool_call_id>'
    session_id   UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('prompt', 'answer')),
    payload      JSONB NOT NULL,            -- {"text": ...} | {"tool_call_id": ..., "answers": ...}
    created_at   timestamptz NOT NULL DEFAULT now(),
    attempts     INT NOT NULL DEFAULT 0,
    not_before   timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,               -- last relay handoff (non-authoritative)
    acked_at     timestamptz                -- confirming event ingested; terminal
);

CREATE INDEX session_outbox_pending
    ON session_outbox (session_id, created_at) WHERE acked_at IS NULL;
