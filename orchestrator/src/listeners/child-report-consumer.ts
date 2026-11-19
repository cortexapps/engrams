/**
 * Reports a spawned child's turn to the session that spawned it.
 *
 * When a child session (a `subsession` task) ends a run, this consumer sends
 * the spawning session a prompt with the child's last assistant message of
 * that run. The parent does not have to poll `wait_sessions` to learn that a
 * child finished.
 *
 * Replay safety: the prompt id is a function of the child session id and the
 * run id, and the coordinator accepts a repeated prompt id as a no-op. While
 * an assistant message waits for its run to end, `handle` returns the held
 * floor (the idx just before that message), so a listener restart replays
 * the message before the run end.
 */

import { Code, ConnectError } from "@connectrpc/connect";
import { and, eq } from "drizzle-orm";

import { sessions } from "../control-plane/client.ts";
import type { CuratedEvent } from "../control-plane/session-events.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "child-report-consumer" });

/** Bound on the child message text that goes into the parent's prompt. A
 *  longer message keeps its end, where a child states its result. */
export const REPORT_MESSAGE_CHARS = 4000;

/** Sends per run end before the consumer stops retrying a transient error. */
const MAX_SEND_ATTEMPTS = 5;

export interface SpawnedChild {
  canonicalTaskName: string;
  taskStatus: string;
  parentSessionId: string;
}

export interface ChildReportConsumerDeps {
  /** The spawned-child task behind `sessionId`, or null for a session that
   *  is not a spawned child. */
  findSpawnedChild(sessionId: string): Promise<SpawnedChild | null>;
  sendPrompt(input: { sessionId: string; promptId: string; text: string }): Promise<void>;
}

export type TurnOutcome = "completed" | "failed" | "interrupted";

interface RunEnd {
  runId: string;
  outcome: TurnOutcome;
}

function parseRunEnd(event: CuratedEvent): RunEnd | undefined {
  if (event.kind !== "run_completed" && event.kind !== "run_interrupted") return undefined;
  let payload: { run_id?: unknown; ok?: unknown } = {};
  try {
    payload = JSON.parse(event.payloadJson) as typeof payload;
  } catch {
    // A malformed payload still ends the run; the event idx names it.
  }
  const runId =
    typeof payload.run_id === "string" && payload.run_id !== ""
      ? payload.run_id
      : `idx-${event.idx}`;
  if (event.kind === "run_interrupted") return { runId, outcome: "interrupted" };
  return { runId, outcome: payload.ok === false ? "failed" : "completed" };
}

function parseAssistantMessage(event: CuratedEvent): { runId: string; text: string } | undefined {
  if (event.kind !== "agent_message") return undefined;
  try {
    const p = JSON.parse(event.payloadJson) as { role?: unknown; run_id?: unknown; text?: unknown };
    return p.role === "assistant" && typeof p.text === "string"
      ? { runId: typeof p.run_id === "string" ? p.run_id : "", text: p.text }
      : undefined;
  } catch {
    return undefined;
  }
}

/** The prompt id of the report for one child run. The coordinator keys
 *  prompts by id, so the same id is never accepted twice. */
export function childReportPromptId(childSessionId: string, runId: string): string {
  return `child_report:${childSessionId}:${runId}`;
}

export function renderChildReport(input: {
  canonicalTaskName: string;
  childSessionId: string;
  outcome: TurnOutcome;
  lastMessage: string | undefined;
}): string {
  const head = `Child session "${input.canonicalTaskName}" (${input.childSessionId}) finished a turn (${input.outcome}).`;
  const message = input.lastMessage?.trim() ?? "";
  let body: string;
  if (message === "") {
    body = "The child sent no assistant message in this turn.";
  } else if (message.length > REPORT_MESSAGE_CHARS) {
    body = `[Truncated: the last ${REPORT_MESSAGE_CHARS} of ${message.length} characters.]\n\n${message.slice(-REPORT_MESSAGE_CHARS)}`;
  } else {
    body = message;
  }
  return `${head}\n\n${body}\n\nUse read_session for its full output.`;
}

/** A rejection that a retry cannot fix: the parent session is gone or ended
 *  (NotFound, or FailedPrecondition for a terminal session), or the prompt id
 *  already belongs to a different prompt. */
function isPermanentSendError(err: unknown): boolean {
  return (
    err instanceof ConnectError &&
    (err.code === Code.NotFound ||
      err.code === Code.FailedPrecondition ||
      err.code === Code.InvalidArgument)
  );
}

export function makeChildReportConsumer(deps: ChildReportConsumerDeps): SessionConsumer {
  // The last assistant message since the previous run end. Every event
  // returns its idx - 1 as the commit while it is held (see the header).
  let held: { runId: string; text: string; idx: bigint } | null = null;
  let applies: boolean | undefined;
  let attemptRunId = "";
  let attempts = 0;

  async function report(run: RunEnd, ctx: { sessionId: string }): Promise<void> {
    // Re-read per run end: the parent can cancel the child at any time.
    const child = await deps.findSpawnedChild(ctx.sessionId);
    if (child === null || child.taskStatus === "cancelled") return;
    const text = renderChildReport({
      canonicalTaskName: child.canonicalTaskName,
      childSessionId: ctx.sessionId,
      outcome: run.outcome,
      lastMessage: held !== null && held.runId === run.runId ? held.text : undefined,
    });
    const promptId = childReportPromptId(ctx.sessionId, run.runId);
    try {
      await deps.sendPrompt({ sessionId: child.parentSessionId, promptId, text });
    } catch (err) {
      if (run.runId !== attemptRunId) {
        attemptRunId = run.runId;
        attempts = 0;
      }
      attempts += 1;
      if (!isPermanentSendError(err) && attempts < MAX_SEND_ATTEMPTS) {
        throw err; // the listener redelivers this event with backoff
      }
      log.warn(
        { sessionId: ctx.sessionId, parentSessionId: child.parentSessionId, promptId, attempts, err },
        "child turn report not sent to the parent session",
      );
      return;
    }
    log.info(
      { sessionId: ctx.sessionId, parentSessionId: child.parentSessionId, promptId },
      "child turn reported to the parent session",
    );
  }

  return {
    name: "child-report",
    // Every kind: an event this consumer is not interested in commits its own
    // idx, and that would move the cursor past a held message.
    interestedIn: () => true,
    async appliesTo(sessionId) {
      applies ??= (await deps.findSpawnedChild(sessionId)) !== null;
      return applies;
    },
    async handle(event, ctx) {
      const message = parseAssistantMessage(event);
      if (message !== undefined) {
        held = { ...message, idx: event.idx };
        return event.idx - 1n;
      }
      const run = parseRunEnd(event);
      if (run === undefined) return held === null ? undefined : held.idx - 1n;
      await report(run, ctx);
      held = null;
    },
  };
}

export function makeProductionChildReportConsumer(): SessionConsumer {
  const db = getDb();
  return makeChildReportConsumer({
    async findSpawnedChild(sessionId) {
      const rows = await db
        .select({
          canonicalTaskName: taskTable.canonicalTaskName,
          taskStatus: taskTable.status,
          parentTaskId: taskTable.parentTaskId,
          spawningSessionId: taskTable.spawningSessionId,
        })
        .from(taskSessionTable)
        .innerJoin(taskTable, eq(taskSessionTable.taskId, taskTable.id))
        .where(and(eq(taskSessionTable.sessionId, sessionId), eq(taskTable.type, "subsession")))
        .limit(1);
      const row = rows[0];
      if (
        row === undefined ||
        row.parentTaskId == null ||
        row.spawningSessionId == null ||
        row.canonicalTaskName == null
      ) {
        return null;
      }
      return {
        canonicalTaskName: row.canonicalTaskName,
        taskStatus: row.taskStatus,
        parentSessionId: row.spawningSessionId,
      };
    },
    async sendPrompt(input) {
      await sessions.sendPrompt(input);
    },
  });
}
