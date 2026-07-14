import { eq } from "drizzle-orm";

import { getDb } from "../db/client.ts";
import { slackSession } from "../db/schema.ts";

export interface SlackSessionStore {
  bind(sessionId: string, threadWfId: string): Promise<void>;
  findThreadWorkflow(sessionId: string): Promise<string | null>;
}

export type SlackSessionDb = ReturnType<typeof getDb>;

export function makeSlackSessionStore(
  db: SlackSessionDb = getDb(),
): SlackSessionStore {
  return {
    async bind(sessionId, threadWfId) {
      await db
        .insert(slackSession)
        .values({ sessionId, threadWfId })
        .onConflictDoUpdate({
          target: slackSession.sessionId,
          set: { threadWfId },
        });
    },

    async findThreadWorkflow(sessionId) {
      const rows = await db
        .select({ threadWfId: slackSession.threadWfId })
        .from(slackSession)
        .where(eq(slackSession.sessionId, sessionId))
        .limit(1);
      return rows[0]?.threadWfId ?? null;
    },
  };
}

export async function bindSlackSession(
  sessionId: string,
  threadWfId: string,
): Promise<void> {
  await makeSlackSessionStore().bind(sessionId, threadWfId);
}
