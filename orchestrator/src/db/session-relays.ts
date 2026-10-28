/** The `automation_session_relay` store (ADR 0119 contract 5): one row per
 * relayed session, holding the place it mirrors into and the render state.
 *
 * Writers: a run's `relay_session` / `run_agent` block (install or
 * re-point, inside a checkpointed step) and the automation consumer (one
 * render per curated event, sequential per session). Both CAS on `version`
 * so a re-point racing a render never loses the other's state; the loser
 * re-reads and renders again.
 */

import { and, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { automationSessionRelay as relayTable } from "./schema.ts";

export interface SessionRelayRow {
  sessionId: string;
  automationId: string;
  provider: string;
  target: Record<string, unknown>;
  state: Record<string, unknown>;
  version: number;
}

export interface SessionRelayStore {
  get(sessionId: string): Promise<SessionRelayRow | null>;
  /** Insert the relay, or return the existing row untouched (the caller
   * re-points through `update`). */
  install(input: {
    sessionId: string;
    automationId: string;
    provider: string;
    target: Record<string, unknown>;
    state: Record<string, unknown>;
  }): Promise<{ row: SessionRelayRow; created: boolean }>;
  /** CAS write: true when `expectVersion` matched and the row advanced. */
  update(
    sessionId: string,
    expectVersion: number,
    patch: { target?: Record<string, unknown>; state?: Record<string, unknown> },
  ): Promise<boolean>;
  remove(sessionId: string): Promise<boolean>;
}

function rowOf(row: typeof relayTable.$inferSelect): SessionRelayRow {
  return {
    sessionId: row.sessionId,
    automationId: row.automationId,
    provider: row.provider,
    target: row.target,
    state: row.state,
    version: row.version,
  };
}

export function makeSessionRelayStore(deps: { db?: ReturnType<typeof getDb>; now?: () => Date } = {}): SessionRelayStore {
  const db = deps.db ?? getDb();
  const now = deps.now ?? (() => new Date());

  async function get(sessionId: string): Promise<SessionRelayRow | null> {
    const [row] = await db.select().from(relayTable).where(eq(relayTable.sessionId, sessionId)).limit(1);
    return row ? rowOf(row) : null;
  }

  return {
    get,
    async install(input) {
      const inserted = await db
        .insert(relayTable)
        .values({
          sessionId: input.sessionId,
          automationId: input.automationId,
          provider: input.provider,
          target: input.target,
          state: input.state,
          createdAt: now(),
          updatedAt: now(),
        })
        .onConflictDoNothing({ target: relayTable.sessionId })
        .returning();
      if (inserted[0]) return { row: rowOf(inserted[0]), created: true };
      const existing = await get(input.sessionId);
      if (existing === null) throw new Error(`session relay "${input.sessionId}" vanished between insert and read`);
      return { row: existing, created: false };
    },
    async update(sessionId, expectVersion, patch) {
      const updated = await db
        .update(relayTable)
        .set({
          ...(patch.target !== undefined ? { target: patch.target } : {}),
          ...(patch.state !== undefined ? { state: patch.state } : {}),
          version: expectVersion + 1,
          updatedAt: now(),
        })
        .where(and(eq(relayTable.sessionId, sessionId), eq(relayTable.version, expectVersion)))
        .returning({ sessionId: relayTable.sessionId });
      return updated.length > 0;
    },
    async remove(sessionId) {
      const deleted = await db
        .delete(relayTable)
        .where(eq(relayTable.sessionId, sessionId))
        .returning({ sessionId: relayTable.sessionId });
      return deleted.length > 0;
    },
  };
}
