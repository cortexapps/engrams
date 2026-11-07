import { and, asc, desc, eq, gt, lte, sql } from "drizzle-orm";
import type { getDb } from "../db/client.ts";
import { mcpExecution } from "../db/schema.ts";
import type { makeMcpEffectDriver } from "./effect-driver.ts";
import { MCP_LIMITS } from "./limits.ts";

const active = sql`${mcpExecution.state} IN ('waiting_approval','authorized','dispatching')`;

/** Independently revisits all active executions; failures do not starve later rows. */
export function makeMcpLifecycleReconciler(deps: {
  db: ReturnType<typeof getDb>;
  driver: Pick<ReturnType<typeof makeMcpEffectDriver>, "reconcile" | "cancel">;
  timeoutMs?: number;
  onSession?(sessionId: string): Promise<void>;
}) {
  let after = "";
  const pending = new Map<string, { promise: Promise<void>; controller: AbortController }>();
  async function check(id: string) {
    let operation = pending.get(id);
    if (!operation) {
      const controller = new AbortController();
      const promise = deps.driver.reconcile(id, controller.signal);
      operation = { promise, controller };
      pending.set(id, operation);
      void promise.then(
        () => pending.delete(id),
        () => pending.delete(id),
      );
    }
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        operation.promise,
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => {
            operation!.controller.abort();
            deps.driver.cancel(id);
            reject(Error("MCP authority check timed out"));
          }, deps.timeoutMs ?? 5_000);
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
  return {
    async reconcileAuthorityChange() {
      const [last] = await deps.db
        .select({ id: mcpExecution.id })
        .from(mcpExecution)
        .where(active)
        .orderBy(desc(mcpExecution.id))
        .limit(1);
      if (!last) return;
      let cursor = "";
      let failed = false;
      for (;;) {
        const rows = await deps.db
          .select({ id: mcpExecution.id })
          .from(mcpExecution)
          .where(and(active, gt(mcpExecution.id, cursor), lte(mcpExecution.id, last.id)))
          .orderBy(asc(mcpExecution.id))
          .limit(MCP_LIMITS.queueSize);
        for (const row of rows) {
          try {
            await check(row.id);
          } catch {
            failed = true;
          }
          cursor = row.id;
        }
        if (rows.length < MCP_LIMITS.queueSize) break;
      }
      if (failed) throw Error("MCP authority reconciliation unavailable");
    },
    async runOnce() {
      const rows = await deps.db
        .select({ id: mcpExecution.id, sessionId: mcpExecution.sessionId })
        .from(mcpExecution)
        .where(and(active, gt(mcpExecution.id, after)))
        .orderBy(asc(mcpExecution.id))
        .limit(MCP_LIMITS.queueSize);
      let failed = false;
      const sessions = new Set<string>();
      for (const row of rows) {
        if (!sessions.has(row.sessionId)) {
          sessions.add(row.sessionId);
          try {
            await deps.onSession?.(row.sessionId);
          } catch {
            failed = true;
          }
        }
        try {
          await check(row.id);
        } catch {
          failed = true;
        }
        after = row.id;
      }
      if (rows.length < MCP_LIMITS.queueSize) after = "";
      if (failed) throw Error("MCP lifecycle reconciliation unavailable");
      return rows.length === MCP_LIMITS.queueSize;
    },
    /** A fixed upper key bounds one event's scan even as new calls arrive. */
    async reconcileSession(sessionId: string) {
      const [last] = await deps.db
        .select({ id: mcpExecution.id })
        .from(mcpExecution)
        .where(and(eq(mcpExecution.sessionId, sessionId), active))
        .orderBy(desc(mcpExecution.id))
        .limit(1);
      if (!last) return;
      let cursor = "";
      for (;;) {
        const rows = await deps.db
          .select({ id: mcpExecution.id })
          .from(mcpExecution)
          .where(
            and(
              eq(mcpExecution.sessionId, sessionId),
              active,
              gt(mcpExecution.id, cursor),
              lte(mcpExecution.id, last.id),
            ),
          )
          .orderBy(asc(mcpExecution.id))
          .limit(MCP_LIMITS.queueSize);
        for (const row of rows) {
          await check(row.id);
          cursor = row.id;
        }
        if (rows.length < MCP_LIMITS.queueSize) return;
      }
    },
  };
}
