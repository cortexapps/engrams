/**
 * SessionIngestWorkflow — the reverse-channel pump (ADR 0060 Decision 1, P1.3).
 *
 * One per session (`workflowID = ingest:<sessionId>[#<epoch>]`, which IS the
 * one-pump-per-session guarantee — no lease). It walks the coordinator's
 * append-only event log forward in bounded reads via `readSessionEventsBounded`
 * and `DBOS.send`s curated content into the owning `SlackThreadWorkflow`'s
 * mailbox, then sends a single terminal message and exits when the session
 * reaches a terminal state.
 *
 * Two invariants this file pins (ADR 0060 §Correctness invariants):
 *  1. **Effect-before-cursor.** Each `DBOS.send` (a checkpointed, replay-once
 *     step) commits BEFORE the local cursor `after` advances. A crash in between
 *     replays the send, which DBOS returns from its checkpoint — no re-send, no
 *     gap. That is what makes the workflow-local cursor safe without a table.
 *  2. **`run_completed` is not terminal.** Only a terminal `status_changed`
 *     (surfaced as `page.terminal` by the reader) ends the loop — a session
 *     re-runs on a follow-up @mention.
 *
 * No `continueAsNew` in the DBOS SDK (divergence from the ADR sketch): to bound
 * `operation_outputs` growth on a long session we self-restart — start a fresh
 * epoch with a deterministic `ingest:<sid>#<epoch+1>` id carrying the cursor,
 * then return. The deterministic id makes the restart idempotent under replay.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { readSessionEventsBounded } from "../control-plane/session-events.ts";
import { THREAD_TOPIC, type ThreadInbox } from "./thread-inbox.ts";

/** Workflow input. `after`/`epoch`/`lastMessage` are set only on a self-restart
 *  (the cursor, epoch, and last-seen assistant message the prior pump handed
 *  off); a fresh pump starts at the log head. */
export interface IngestInput {
  sessionId: string;
  threadWfId: string;
  after?: bigint;
  epoch?: number;
  lastMessage?: string;
}

/** Poll cadence when a bounded read returned no new curated content (tail). */
const POLL_INTERVAL_MS = 1_000;
/** Self-restart after this many iterations to bound `operation_outputs`. */
const RESTART_AFTER_ITERATIONS = 500;

/**
 * One bounded read of the log, as a checkpointed step: non-deterministic
 * (reads external state) by design, so its result is recorded once and
 * returned verbatim on replay.
 */
const readPage = DBOS.registerStep(
  (sessionId: string, after: bigint) => readSessionEventsBounded(sessionId, after),
  { name: "ingest-read-page" },
);

async function sessionIngestWorkflowImpl(input: IngestInput): Promise<void> {
  const { sessionId, threadWfId } = input;
  let after: bigint = input.after ?? -1n;
  const epoch = input.epoch ?? 0;
  // The session's most-recent assistant message seen so far — carried across a
  // self-restart so the closing summary stays correct even past a history bound.
  let lastMessage: string | undefined = input.lastMessage;

  for (let i = 0; i < RESTART_AFTER_ITERATIONS; i++) {
    const page = await readPage(sessionId, after);

    // Effect-before-cursor: forward each curated event (replay-once send)
    // BEFORE advancing the cursor.
    for (const ev of page.events) {
      await DBOS.send<ThreadInbox>(threadWfId, { kind: "session_event", event: ev }, THREAD_TOPIC);
    }
    after = page.nextAfter;
    if (page.lastAssistantText !== undefined) lastMessage = page.lastAssistantText;

    if (page.terminal) {
      await DBOS.send<ThreadInbox>(
        threadWfId,
        { kind: "session_terminal", outcome: page.terminal.outcome, ...(lastMessage ? { lastMessage } : {}) },
        THREAD_TOPIC,
      );
      return;
    }

    // At the tail (no new content) — wait before the next bounded read.
    if (page.events.length === 0) {
      await DBOS.sleep(POLL_INTERVAL_MS);
    }
  }

  // History bound reached: hand the cursor to a fresh epoch and exit. The
  // deterministic id makes this restart idempotent under replay.
  await DBOS.startWorkflow(sessionIngestWorkflow, {
    workflowID: `ingest:${sessionId}#${epoch + 1}`,
  })({ sessionId, threadWfId, after, epoch: epoch + 1, ...(lastMessage ? { lastMessage } : {}) });
}

export const sessionIngestWorkflow = DBOS.registerWorkflow(sessionIngestWorkflowImpl, {
  name: "SessionIngestWorkflow",
});
