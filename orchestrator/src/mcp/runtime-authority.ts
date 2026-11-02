import { McpAuthorityStale } from "./authority-error.ts";
import { subject } from "@casl/ability";
import { eq } from "drizzle-orm";
import { automation, automationRun, automationSession, mcpExecution } from "../db/schema.ts";
import { abilityFor } from "../authz/ability.ts";
import { canonicalDigest } from "./hash.ts";
import {
  assertLocalReviewCurrent,
  lockHumanSession,
  lockSessionChain,
  type JournalTx,
} from "./journal-review.ts";

/** Local authorization stays in the journal transaction. No network or cached role is accepted. */
export async function lockRuntimeAuthority(
  tx: JournalTx,
  call: typeof mcpExecution.$inferSelect,
  connectionLock: "share" | "update" = "update",
) {
  let authority: Awaited<ReturnType<typeof lockSessionChain>>;
  if (call.principalId.startsWith("automation:")) {
    const id = call.principalId.slice("automation:".length);
    const [linked] = await tx
      .select()
      .from(automationSession)
      .where(eq(automationSession.sessionId, call.sessionId));
    if (!linked || !id) throw new McpAuthorityStale("automation identity unavailable");
    const [owner] = await tx.select().from(automation).where(eq(automation.id, id)).for("share");
    const [run] = await tx
      .select()
      .from(automationRun)
      .where(eq(automationRun.id, linked.runId))
      .for("share");
    const [session] = await tx
      .select()
      .from(automationSession)
      .where(eq(automationSession.sessionId, call.sessionId))
      .for("share");
    if (
      !owner?.enabled ||
      owner.archivedAt ||
      !run ||
      run.automationId !== id ||
      run.status !== "running" ||
      run.dryRun ||
      run.endedAt ||
      session?.runId !== run.id ||
      (session.role === "primary" &&
        (run.sessionId !== call.sessionId || run.taskId !== call.taskId))
    )
      throw new McpAuthorityStale("automation authority unavailable");
    authority = await lockSessionChain(tx, call.sessionId);
    if (call.classification === "write") {
      const resolved = call.requestContext?.resolvedTool;
      const publication = authority.session.mcpBindingEnvelope?.profilePublication;
      if (
        call.authorizationKind !== "automation" ||
        !resolved?.effectiveGrant.automationWrite ||
        !publication ||
        !Number.isSafeInteger(publication.revision) ||
        publication.revision < 1 ||
        !publication.actorId ||
        publication.grantsHash !== authority.session.mcpBindingEnvelope?.profileGrantsHash ||
        canonicalDigest(resolved.effectiveGrant) !== call.automationGrantHash
      )
        throw new McpAuthorityStale("explicit published automation write grant required");
    }
  } else {
    const human = await lockHumanSession(tx, call.sessionId, call.principalId, true);
    const ability = abilityFor({ id: human.actor.id, role: human.actor.role ?? "user" });
    const owner = { createdByUserId: human.owner.createdByUserId };
    if (
      !ability.can("update", subject("Task", { ...owner })) ||
      !ability.can("prompt", subject("Session", { ...owner })) ||
      call.authorizationKind === "automation"
    )
      throw new McpAuthorityStale("current session permission required");
    authority = human;
  }
  if (
    (authority.session.mcpManagedLifecycle != null &&
      authority.session.mcpManagedLifecycle.phase !== "started") ||
    authority.session.profileId !== authority.session.mcpBindingEnvelope?.profileId ||
    authority.session.integrationPrincipalId !== call.principalId ||
    authority.session.taskId !== call.taskId ||
    ["done", "failed", "cancelled"].includes(authority.owner.status)
  )
    throw new McpAuthorityStale("current session principal required");
  await assertLocalReviewCurrent(tx, call, authority.session, authority.ancestors, connectionLock);
  return authority;
}
