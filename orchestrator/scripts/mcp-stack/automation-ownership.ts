import { readdir } from "node:fs/promises";
import { join } from "node:path";
import { z } from "zod";
import {
  privateRoot,
  put,
  readOwned,
} from "../../../tests/fixtures/mcp-stack/platform/owned.ts";
import { requireThat, sha256, type Context } from "./context.ts";
import { canonicalJson } from "./guest.ts";
import type { PublicClient } from "./transport.ts";

type OwnershipContext = Pick<
  Context,
  "runRoot" | "runId" | "sourceCommit" | "hostId"
>;
const id = z.string().regex(/^[A-Za-z0-9_-]{1,96}$/);
const reference = z.strictObject({
  automationId: id,
  definitionActorId: id,
  definitionSha256: z.string().regex(/^[a-f0-9]{64}$/),
  profileId: id,
  runId: z
    .string()
    .regex(/^[A-Za-z0-9_:-]{1,256}$/)
    .optional(),
  sessionId: z.uuid().optional(),
  taskId: z.string().min(1).max(256).optional(),
});
const receiptSchema = reference.extend({
  version: z.literal(1),
  fixtureRunId: id,
  sourceCommit: z.string().regex(/^[a-f0-9]{40}$/),
  hostId: z.uuid(),
});
type Receipt = z.infer<typeof receiptSchema>;
export type OwnedAutomationRun = Receipt & { runId: string };
export type ManagedAutomationSession = {
  sessionId: string;
  actorId: string;
  hostId: string;
  taskId: string;
};
export type AutomationCleanupClient = {
  automations: Pick<PublicClient["automations"], "getAutomation">;
  automationRuns: Pick<PublicClient["automationRuns"], "getRun">;
  tasks: Pick<PublicClient["tasks"], "getTask">;
};
function receiptPath(
  ctx: OwnershipContext,
  receipt: z.infer<typeof reference>,
) {
  return join(
    ctx.runRoot,
    receipt.sessionId && receipt.runId
      ? `automation-session-owner-${sha256(receipt.runId)}.json`
      : receipt.runId
        ? `automation-run-owner-${sha256(receipt.runId)}.json`
        : `automation-owner-${sha256(receipt.automationId)}.json`,
  );
}
async function readReceipt(
  ctx: OwnershipContext,
  path: string,
): Promise<Receipt> {
  const receipt = receiptSchema.parse(
    JSON.parse((await readOwned(path, 4096)).toString()),
  );
  requireThat(
    Boolean(receipt.sessionId) === Boolean(receipt.taskId) &&
      (!receipt.sessionId || Boolean(receipt.runId)),
    "automation_ownership_session_reference",
  );
  requireThat(
    receipt.fixtureRunId === ctx.runId &&
      receipt.sourceCommit === ctx.sourceCommit &&
      receipt.hostId === ctx.hostId &&
      receiptPath(ctx, receipt) === path,
    "automation_ownership_receipt_identity",
  );
  return receipt;
}
async function matchCreation(ctx: OwnershipContext, receipt: Receipt) {
  const creation = { ...receipt };
  delete creation.runId;
  delete creation.sessionId;
  delete creation.taskId;
  const actual = await readReceipt(ctx, receiptPath(ctx, creation));
  requireThat(
    canonicalJson(actual) === canonicalJson(creation),
    "automation_ownership_creation",
  );
}
async function matchRun(ctx: OwnershipContext, receipt: Receipt) {
  const run = { ...receipt };
  delete run.sessionId;
  delete run.taskId;
  const actual = await readReceipt(ctx, receiptPath(ctx, run));
  requireThat(
    canonicalJson(actual) === canonicalJson(run),
    "automation_ownership_run_reference",
  );
}
export async function recordOwnedAutomation(
  ctx: OwnershipContext,
  owned: z.infer<typeof reference>,
): Promise<void> {
  const receipt = receiptSchema.parse({
    ...reference.parse(owned),
    version: 1,
    fixtureRunId: ctx.runId,
    sourceCommit: ctx.sourceCommit,
    hostId: ctx.hostId,
  });
  requireThat(
    Boolean(receipt.sessionId) === Boolean(receipt.taskId) &&
      (!receipt.sessionId || Boolean(receipt.runId)),
    "automation_ownership_session_reference",
  );
  if (receipt.runId) await matchCreation(ctx, receipt);
  if (receipt.sessionId) await matchRun(ctx, receipt);
  await put(receiptPath(ctx, receipt), JSON.stringify(receipt));
}
export async function readOwnedAutomationRuns(
  ctx: OwnershipContext,
): Promise<OwnedAutomationRun[]> {
  await privateRoot(ctx.runRoot);
  const allNames = new Set(await readdir(ctx.runRoot));
  const names = [...allNames].filter((name) =>
    /^automation-run-owner-[a-f0-9]{64}\.json$/.test(name),
  );
  requireThat(names.length <= 128, "automation_ownership_receipt_bound");
  const runs: OwnedAutomationRun[] = [];
  for (const name of names.sort()) {
    let receipt = await readReceipt(ctx, join(ctx.runRoot, name));
    requireThat(receipt.runId, "automation_ownership_run");
    await matchCreation(ctx, receipt);
    const sessionName = `automation-session-owner-${sha256(receipt.runId)}.json`;
    if (allNames.has(sessionName)) {
      receipt = await readReceipt(ctx, join(ctx.runRoot, sessionName));
      requireThat(
        receipt.runId && receipt.sessionId && receipt.taskId,
        "automation_ownership_session_reference",
      );
      await matchCreation(ctx, receipt);
      await matchRun(ctx, receipt);
    }
    requireThat(receipt.runId, "automation_ownership_run");
    runs.push({ ...receipt, runId: receipt.runId });
  }
  return runs;
}

/** Cleanup ownership only. A terminal run does not give runtime authority. */
export async function ownedAutomationCleanupSessions(
  ctx: OwnershipContext,
  client: AutomationCleanupClient,
  managed: ReadonlyMap<string, ManagedAutomationSession>,
  actors: ReadonlySet<string>,
  candidates: ReadonlySet<string>,
): Promise<Set<string>> {
  const sessions = new Set<string>();
  for (const receipt of await readOwnedAutomationRuns(ctx)) {
    if (receipt.sessionId && !candidates.has(receipt.sessionId)) continue;
    requireThat(
      actors.has(receipt.definitionActorId),
      "cleanup_automation_creator",
    );
    const { automation } = await client.automations.getAutomation({
      lookup: { case: "id", value: receipt.automationId },
    });
    requireThat(
      automation?.id === receipt.automationId &&
        automation.kind === "user" &&
        automation.createdByUserId === receipt.definitionActorId &&
        automation.version &&
        automation.version.automationId === receipt.automationId &&
        automation.version.createdByUserId === receipt.definitionActorId &&
        automation.version.number === automation.currentVersion &&
        automation.version.definitionJson.length <= 65536,
      "cleanup_automation_definition",
    );
    const definition = JSON.parse(automation.version.definitionJson);
    requireThat(
      sha256(canonicalJson(definition)) === receipt.definitionSha256,
      "cleanup_automation_definition_hash",
    );
    const blocks = z
      .object({
        blocks: z
          .array(
            z.object({
              id: z.string(),
              type: z.string(),
              config: z.record(z.string(), z.unknown()),
            }),
          )
          .max(128),
      })
      .parse(definition).blocks;
    const launch = blocks.filter(
      (block) => block.id === "launch" && block.type === "create_session",
    );
    requireThat(
      launch.length === 1 && launch[0]!.config.profileId === receipt.profileId,
      "cleanup_automation_profile",
    );
    const { run } = await client.automationRuns.getRun({
      runId: receipt.runId,
    });
    requireThat(
      run?.brief?.id === receipt.runId &&
        run.brief.automationId === receipt.automationId &&
        run.brief.version === automation.version.number &&
        run.brief.triggerSource === "manual" &&
        !run.brief.dryRun &&
        run.steps.length <= 128 &&
        run.sessionIds.length <= 128,
      "cleanup_automation_run",
    );
    const steps = run.steps.filter(
      (step) => step.blockId === "launch" && step.status === "succeeded",
    );
    if (steps.length === 0 && run.sessionIds.length === 0) continue;
    requireThat(
      steps.length === 1 && steps[0]!.outputsJson.length <= 65536,
      "cleanup_automation_launch",
    );
    const output = z
      .object({
        session_id: z.uuid(),
        task_id: z.string().min(1).max(256),
        initial_prompt: z.literal(false),
        prompt: z.literal(""),
      })
      .parse(JSON.parse(steps[0]!.outputsJson));
    if (!candidates.has(output.session_id)) continue;
    requireThat(
      !receipt.sessionId ||
        (receipt.sessionId === output.session_id &&
          receipt.taskId === output.task_id),
      "cleanup_automation_recorded_session",
    );
    requireThat(
      run.sessionIds.length === 1 &&
        run.sessionIds[0] === output.session_id &&
        steps[0]!.sessionId === output.session_id,
      "cleanup_automation_session",
    );
    const { task } = await client.tasks.getTask({ taskId: output.task_id });
    requireThat(
      task?.id === output.task_id &&
        task.type === "automation" &&
        (task.rootTaskId ?? task.id) === output.task_id &&
        task.sessions.length <= 128 &&
        task.sourceJson.length <= 65536,
      "cleanup_automation_task",
    );
    const source = z
      .object({
        provider: z.literal("automation"),
        automationId: z.string(),
        runId: z.string(),
      })
      .parse(JSON.parse(task.sourceJson));
    requireThat(
      source.automationId === receipt.automationId &&
        source.runId === receipt.runId,
      "cleanup_automation_task_source",
    );
    const primary = task.sessions.filter(
      (ref) => ref.sessionId === output.session_id && ref.role === "primary",
    );
    requireThat(
      primary.length === 1 &&
        (!primary[0]!.session ||
          (primary[0]!.session.id === output.session_id &&
            (!primary[0]!.session.hostId ||
              primary[0]!.session.hostId === ctx.hostId))),
      "cleanup_automation_task_session",
    );
    const boot = managed.get(output.session_id);
    requireThat(
      !boot ||
        (boot.sessionId === output.session_id &&
          boot.hostId === ctx.hostId &&
          boot.actorId === `automation:${receipt.automationId}` &&
          boot.taskId === output.task_id),
      "cleanup_automation_managed_identity",
    );
    requireThat(
      !sessions.has(output.session_id),
      "cleanup_automation_duplicate_session",
    );
    sessions.add(output.session_id);
  }
  return sessions;
}
