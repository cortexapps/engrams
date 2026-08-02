import { and, eq, isNull, lt } from "drizzle-orm";

import { getDb } from "../db/client.ts";
import { pendingToolCall as pendingToolCallTable } from "../db/schema.ts";
import type { ToolHandling } from "./registry.ts";

export interface PendingToolCallInput {
  sessionId: string;
  toolCallId: string;
  toolName: string;
  handling: ToolHandling;
  requestedAt: Date;
}

export interface PendingToolCallRow extends PendingToolCallInput {
  submittedAt: Date | null;
  completedAt: Date | null;
}

export interface PendingToolCallStore {
  recordRequested(input: PendingToolCallInput): Promise<void>;
  markSubmitted(sessionId: string, toolCallId: string, at: Date): Promise<void>;
  markCompleted(sessionId: string, toolCallId: string, at: Date): Promise<void>;
  find(sessionId: string, toolCallId: string): Promise<PendingToolCallRow | null>;
  listUnsubmittedSessionCallsBefore(cutoff: Date): Promise<PendingToolCallRow[]>;
  /** ADR 0107: session_ids with a requested-but-unsubmitted session-handled
   *  call — a plan awaiting review or an unanswered question. Powers the
   *  task list's `awaiting_review` derivation. */
  listSessionIdsWithPendingSessionCalls(): Promise<string[]>;
}

/** Drizzle handle narrowed by usage; exported so tests can pass a hand-rolled
 *  recording fluent fake without a live database. */
export type PendingToolCallDb = ReturnType<typeof getDb>;

function toRow(row: typeof pendingToolCallTable.$inferSelect): PendingToolCallRow {
  return {
    sessionId: row.sessionId,
    toolCallId: row.toolCallId,
    toolName: row.toolName,
    handling: row.handling === "session" ? "session" : "handled",
    requestedAt: row.requestedAt,
    submittedAt: row.submittedAt ?? null,
    completedAt: row.completedAt ?? null,
  };
}

export function makePendingToolCallStore(
  db: PendingToolCallDb = getDb(),
): PendingToolCallStore {
  return {
    async recordRequested(input) {
      await db
        .insert(pendingToolCallTable)
        .values(input)
        .onConflictDoNothing({
          target: [pendingToolCallTable.sessionId, pendingToolCallTable.toolCallId],
        });
    },

    async markSubmitted(sessionId, toolCallId, at) {
      await db
        .update(pendingToolCallTable)
        .set({ submittedAt: at })
        .where(
          and(
            eq(pendingToolCallTable.sessionId, sessionId),
            eq(pendingToolCallTable.toolCallId, toolCallId),
          ),
        );
    },

    async markCompleted(sessionId, toolCallId, at) {
      await db
        .update(pendingToolCallTable)
        .set({ completedAt: at })
        .where(
          and(
            eq(pendingToolCallTable.sessionId, sessionId),
            eq(pendingToolCallTable.toolCallId, toolCallId),
          ),
        );
    },

    async find(sessionId, toolCallId) {
      const rows = await db
        .select()
        .from(pendingToolCallTable)
        .where(
          and(
            eq(pendingToolCallTable.sessionId, sessionId),
            eq(pendingToolCallTable.toolCallId, toolCallId),
          ),
        )
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

    async listSessionIdsWithPendingSessionCalls() {
      const rows = await db
        .selectDistinct({ sessionId: pendingToolCallTable.sessionId })
        .from(pendingToolCallTable)
        .where(
          and(
            eq(pendingToolCallTable.handling, "session"),
            isNull(pendingToolCallTable.submittedAt),
          ),
        );
      return rows.map((row) => row.sessionId);
    },

    async listUnsubmittedSessionCallsBefore(cutoff) {
      const rows = await db
        .select()
        .from(pendingToolCallTable)
        .where(
          and(
            eq(pendingToolCallTable.handling, "session"),
            lt(pendingToolCallTable.requestedAt, cutoff),
            isNull(pendingToolCallTable.submittedAt),
          ),
        );
      return rows.map(toRow);
    },
  };
}
