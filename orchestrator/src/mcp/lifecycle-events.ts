import { and, eq } from "drizzle-orm";
import type { getDb } from "../db/client.ts";
import { consumerCursor } from "../db/schema.ts";
import { parseStatusChangeTo } from "../control-plane/session-events.ts";

export type LifecycleEvent = { idx: bigint; kind: string; payloadJson: string };
const consumer = "mcp-lifecycle";

/** One ordered page. Current-authority reconciliation commits before the cursor moves to the next event. */
export function makeMcpLifecycleEvents(deps: {
  db: ReturnType<typeof getDb>;
  now(): Date;
  list(sessionId: string, after: bigint): Promise<LifecycleEvent[]>;
  reconcile(sessionId: string, event: LifecycleEvent): Promise<void>;
}) {
  return {
    async runOnce(sessionId: string) {
      await deps.db
        .insert(consumerCursor)
        .values({ sessionId, consumer, lastIdx: -1n, updatedAt: deps.now() })
        .onConflictDoNothing();
      const [cursor] = await deps.db
        .select()
        .from(consumerCursor)
        .where(and(eq(consumerCursor.sessionId, sessionId), eq(consumerCursor.consumer, consumer)));
      let previous = cursor!.lastIdx;
      const events = await deps.list(sessionId, previous);
      if (events.length > 200) throw Error("MCP lifecycle page too large");
      let last = previous;
      for (const event of events) {
        if (event.idx <= 0n || event.idx <= last) throw Error("invalid MCP lifecycle event order");
        last = event.idx;
      }
      for (const event of events) {
        const status =
          event.kind === "status_changed" ? parseStatusChangeTo(event.payloadJson) : undefined;
        if (event.kind === "status_changed" && !status) throw Error("invalid MCP lifecycle status");
        if (
          event.kind === "run_interrupted" ||
          event.kind === "resumed" ||
          (status && ["completed", "failed", "dead"].includes(status))
        )
          await deps.reconcile(sessionId, event);
        const moved = await deps.db
          .update(consumerCursor)
          .set({ lastIdx: event.idx, updatedAt: deps.now() })
          .where(
            and(
              eq(consumerCursor.sessionId, sessionId),
              eq(consumerCursor.consumer, consumer),
              eq(consumerCursor.lastIdx, previous),
            ),
          )
          .returning({ lastIdx: consumerCursor.lastIdx });
        if (!moved.length) return;
        previous = event.idx;
      }
    },
  };
}
