import { eq } from "drizzle-orm";
import type { getDb } from "../db/client.ts";
import { user } from "../db/schema.ts";
import { isServiceAccountEmail } from "../rpc/api-key.ts";

type PublisherTx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** Lock the actor before publication targets so demotion and publication serialize. */
export async function lockMcpPublisher(tx: PublisherTx, actorId: string) {
  const [actor] = await tx.select().from(user).where(eq(user.id, actorId)).for("share");
  if (!actor || actor.banned || actor.role !== "admin" || isServiceAccountEmail(actor.email))
    throw new Error("current human administrator required for MCP publication");
}
