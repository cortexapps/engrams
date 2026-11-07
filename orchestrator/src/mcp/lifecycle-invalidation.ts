import { and, asc, eq, gt, inArray } from "drizzle-orm";
import type { getDb } from "../db/client.ts";
import { mcpExecution } from "../db/schema.ts";
import { makeMcpJournal, type McpInternalCancellationCause } from "./journal.ts";
import { MCP_LIMITS } from "./limits.ts";

/** Internal causes carry no human actor. Each cancellation is durable before local abort. */
export function makeMcpLifecycleInvalidator(deps: {
  db: ReturnType<typeof getDb>;
  abort(id: string): Promise<void>;
  now?: () => Date;
}) {
  const journal = makeMcpJournal(deps.db);
  return async (sessionId: string, cause: McpInternalCancellationCause) => {
    let after = "";
    for (;;) {
      const rows = await deps.db
        .select({ id: mcpExecution.id })
        .from(mcpExecution)
        .where(
          and(
            eq(mcpExecution.sessionId, sessionId),
            gt(mcpExecution.id, after),
            inArray(mcpExecution.state, ["waiting_approval", "authorized", "dispatching"]),
          ),
        )
        .orderBy(asc(mcpExecution.id))
        .limit(MCP_LIMITS.queueSize);
      for (const row of rows) {
        await journal.cancelInternal(row.id, cause, (deps.now ?? (() => new Date()))());
        await deps.abort(row.id);
        after = row.id;
      }
      if (rows.length < MCP_LIMITS.queueSize) return;
    }
  };
}
