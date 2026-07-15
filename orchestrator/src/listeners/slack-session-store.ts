import { eq } from "drizzle-orm";

import { getDb } from "../db/client.ts";
import { slackSession } from "../db/schema.ts";

export interface SlackSessionStore {
  findThreadWorkflow(sessionId: string): Promise<string | null>;
}

export type SlackSessionDb = ReturnType<typeof getDb>;

export function makeSlackSessionStore(
  db: SlackSessionDb = getDb(),
): SlackSessionStore {
  return {
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
