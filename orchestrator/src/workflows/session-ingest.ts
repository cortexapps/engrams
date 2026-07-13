/**
 * SessionIngestWorkflow — the Slack reverse-channel pump (ADR 0060 Decision 1,
 * P1.3).
 *
 * One per Slack-backed session (`workflowID = ingest:<sessionId>[#<epoch>]`).
 * It walks the coordinator's append-only event log forward in bounded reads
 * via `readSessionEventsBounded` and `DBOS.send`s curated content into the
 * owning `SlackThreadWorkflow`'s mailbox, then sends a single terminal message
 * and exits when the session reaches a terminal state.
 *
 * This workflow is deliberately surface-only. The independent per-session
 * ToolDispatchWorkflow owns generic tool bookkeeping and handled-tool
 * dispatch, so a Slack session can run both pumps without dispatching twice.
 *
 * Two invariants this file pins (ADR 0060 §Correctness invariants):
 *  1. **Effect-before-cursor.** Each `DBOS.send` commits before the local cursor
 *     advances. A crash in between replays the checkpointed send — no re-send,
 *     no gap.
 *  2. **`run_completed` is not terminal.** Only a terminal `status_changed`
 *     (surfaced as `page.terminal` by the reader) ends the loop.
 *
 * DBOS has no `continueAsNew`, so a deterministic fresh epoch bounds
 * `operation_outputs` growth on long sessions.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { readSessionEventsBounded } from "../control-plane/session-events.ts";
import { THREAD_TOPIC, type ThreadInbox } from "./thread-inbox.ts";

/** Workflow input. Restart-only fields carry the prior pump's cursor, epoch,
 * and most-recent assistant message into its deterministic successor. */
export interface IngestInput {
  sessionId: string;
  threadWfId: string;
  after?: bigint;
  epoch?: number;
  lastMessage?: string;
}

const POLL_INTERVAL_MS = 1_000;
const RESTART_AFTER_ITERATIONS = 500;

/** One bounded external read, checkpointed once for deterministic replay. */
const readPage = DBOS.registerStep(
  (sessionId: string, after: bigint) => readSessionEventsBounded(sessionId, after),
  { name: "ingest-read-page" },
);

async function sessionIngestWorkflowImpl(input: IngestInput): Promise<void> {
  const { sessionId, threadWfId } = input;
  let after = input.after ?? -1n;
  const epoch = input.epoch ?? 0;
  let lastMessage = input.lastMessage;

  for (let i = 0; i < RESTART_AFTER_ITERATIONS; i++) {
    const page = await readPage(sessionId, after);

    // Effect-before-cursor: every curated send finishes before `after` moves.
    for (const event of page.events) {
      await DBOS.send<ThreadInbox>(
        threadWfId,
        { kind: "session_event", event },
        THREAD_TOPIC,
      );
    }
    after = page.nextAfter;
    if (page.lastAssistantText !== undefined) lastMessage = page.lastAssistantText;

    if (page.terminal) {
      await DBOS.send<ThreadInbox>(
        threadWfId,
        {
          kind: "session_terminal",
          outcome: page.terminal.outcome,
          ...(lastMessage ? { lastMessage } : {}),
        },
        THREAD_TOPIC,
      );
      return;
    }

    if (page.events.length === 0) {
      await DBOS.sleep(POLL_INTERVAL_MS);
    }
  }

  await DBOS.startWorkflow(sessionIngestWorkflow, {
    workflowID: `ingest:${sessionId}#${epoch + 1}`,
  })({
    sessionId,
    threadWfId,
    after,
    epoch: epoch + 1,
    ...(lastMessage ? { lastMessage } : {}),
  });
}

export const sessionIngestWorkflow = DBOS.registerWorkflow(sessionIngestWorkflowImpl, {
  name: "SessionIngestWorkflow",
});
