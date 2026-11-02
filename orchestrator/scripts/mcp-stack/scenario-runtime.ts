import { Code } from "@connectrpc/connect";
import { z } from "zod";
import {
  bindingTool,
  createCatalog,
  createFixtureProfile,
  createHumanTask,
  expectRejected,
  waitFor,
  type bootstrapHumans,
} from "./bootstrap.ts";
import {
  requireThat,
  sha256,
  type Context,
  type FixtureKind,
} from "./context.ts";
import {
  canonicalJson,
  guestReady,
  proofPaths,
  readGuestProof,
} from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import type { PublicClient } from "./transport.ts";

export type Principals = Awaited<ReturnType<typeof bootstrapHumans>>;
export type StartedScenario = Awaited<ReturnType<typeof startScenario>>;
export async function startScenario(
  ctx: Context,
  principals: Principals,
  database: DatabaseObserver,
  input: {
    harness: Harness;
    suffix: string;
    protocol: FixtureKind;
    upstreamName: "read_counter" | "write_counter";
    mode: "read" | "write" | "denied" | "refire" | "held-call" | "forged";
    caller: "owner" | "unattended";
    automationWrite: boolean;
    arguments: Record<string, unknown>;
    constraints: object[];
    callId: string;
    defaultAllow?: boolean;
    expectedNativeResult?: Record<string, unknown>;
  },
) {
  const fixture = new FixtureObserver(ctx, input.protocol);
  const before = await fixture.observe();
  const { connection, catalog } = await createCatalog(
    ctx,
    principals.admin,
    input.suffix,
    input.protocol,
  );
  const profile = await createFixtureProfile(ctx, principals.admin, {
    ...input,
    connectionId: connection.id,
    connectionAlias: connection.alias,
    grant: true,
    defaultAllow: input.defaultAllow ?? false,
  });
  const client = principals[input.caller];
  const task = await createHumanTask(
    ctx,
    client,
    profile.profileId,
    input.harness,
  );
  database.own(task.taskId, task.sessionId);
  const coordinator = await waitFor(
    ctx,
    "coordinator_started",
    () => database.started(task.sessionId, ctx.hostId),
    (state) => state.phase === "started",
  );
  requireThat(
    coordinator.source_key.context.operator_identity ===
      ctx.publication.candidateIdentity &&
      coordinator.source_key.context.target.session_id === task.sessionId &&
      coordinator.source_key.context.target.host_id === ctx.hostId,
    "started_target_publication",
  );
  await waitFor(
    ctx,
    "orchestrator_started",
    () => database.managedLifecycle(task.sessionId, coordinator),
    (state) => state.phase === "started",
  );
  const bound = await bindingTool(
    client,
    task.sessionId,
    connection.id,
    input.upstreamName,
  );
  const reviewed = catalog.tools.find(
    (tool) => tool.upstreamName === input.upstreamName,
  );
  requireThat(
    bound.modelToolName === profile.alias &&
      bound.reviewedTool &&
      reviewed &&
      canonicalJson(JSON.parse(bound.reviewedTool.inputSchemaJson)) ===
        canonicalJson(JSON.parse(reviewed.inputSchemaJson)),
    "exact_delivered_binding_schema",
  );
  await guestReady(ctx, client, task.sessionId);
  return {
    ...task,
    ...profile,
    client,
    fixture,
    connection,
    catalog,
    before,
    coordinator,
    input,
  };
}
export async function sendScenarioPrompt(
  scenario: StartedScenario,
  plan = false,
) {
  await scenario.client.sessions.sendPrompt({
    sessionId: scenario.sessionId,
    text: "Run the neutral fixture call.",
    promptId: `fixture-${scenario.input.suffix}`,
    ...(plan ? { harnessMode: "plan" } : {}),
  });
}
export async function execution(
  ctx: Context,
  scenario: StartedScenario,
  terminal = true,
) {
  return waitFor(
    ctx,
    "public_execution",
    async () => {
      const state = await scenario.client.integration.getSessionMcpState({
        sessionId: scenario.sessionId,
        omitBindings: true,
        pageSize: 64,
      });
      requireThat(!state.hasMoreExecutions, "fresh_session_execution_bound");
      const matches = state.executions.filter(
        (row) =>
          row.toolCallId === scenario.input.callId &&
          row.connectionId === scenario.connection.id,
      );
      requireThat(matches.length <= 1, "single_call_identity");
      return matches[0];
    },
    (value) =>
      !!value &&
      (!terminal ||
        ["succeeded", "failed", "canceled", "outcome_unknown"].includes(
          value.state,
        )),
  );
}
export async function approveOnce(
  ctx: Context,
  principals: Principals,
  scenario: StartedScenario,
  actor: "owner" | "admin",
) {
  const pending = await waitFor(
    ctx,
    "interactive_approval",
    async () => {
      const response = await scenario.client.integration.listMcpApprovals({
        sessionId: scenario.sessionId,
      });
      requireThat(
        !response.nextExecutionId && response.approvals.length <= 1,
        "single_approval",
      );
      return response.approvals[0];
    },
    (value) => !!value,
  );
  requireThat(
    pending?.expectedReviewContext &&
      pending.toolCallId === scenario.input.callId &&
      pending.sessionId === scenario.sessionId &&
      pending.connectionId === scenario.connection.id,
    "exact_approval_context",
  );
  const reviewContext = pending.expectedReviewContext;
  const request = {
    executionId: pending.executionId,
    approve: true,
    expectedReviewContext: reviewContext,
  };
  await expectRejected(
    () => principals.other.integration.decideMcpApproval(request),
    [Code.PermissionDenied, Code.NotFound],
  );
  await expectRejected(
    () =>
      principals[actor].integration.decideMcpApproval({
        ...request,
        expectedReviewContext: {
          ...reviewContext,
          argumentHash: `sha256:${"0".repeat(64)}`,
        },
      }),
    [Code.Aborted, Code.FailedPrecondition],
  );
  const approved =
    await principals[actor].integration.decideMcpApproval(request);
  requireThat(
    approved.receipt?.executionId === pending.executionId &&
      approved.receipt.decision === "approved",
    "owner_admin_approval_receipt",
  );
  await expectRejected(
    () =>
      principals[actor].integration.decideMcpApproval({
        ...request,
        approve: false,
      }),
    [Code.Aborted, Code.FailedPrecondition],
  );
  await expectRejected(
    () => principals[actor].integration.decideMcpApproval(request),
    [Code.Aborted, Code.FailedPrecondition],
  );
  return {
    executionId: pending.executionId,
    contextHash: sha256(canonicalJson(pending.expectedReviewContext)),
  };
}
export async function verifiedCounterResult(
  scenario: StartedScenario,
  executionId: string,
  count: number,
  isError: boolean,
) {
  const response = await scenario.client.integration.getMcpExecutionResult({
    sessionId: scenario.sessionId,
    executionId,
  });
  requireThat(
    response.resultJson && !response.resultUnavailable,
    "persisted_verified_result_available",
  );
  const parsed = z
    .strictObject({
      outcome: z.enum(["succeeded", "failed"]),
      delivery: z.literal("available"),
      output: z.strictObject({
        content: z.array(
          z.strictObject({ type: z.literal("text"), text: z.string() }),
        ),
        structuredContent: z.strictObject({ count: z.number().int() }),
        isError: z.boolean(),
      }),
    })
    .safeParse(JSON.parse(response.resultJson));
  requireThat(
    parsed.success &&
      parsed.data.outcome === (isError ? "failed" : "succeeded") &&
      response.state === parsed.data.outcome &&
      canonicalJson(parsed.data.output) ===
        canonicalJson({
          content: [{ type: "text", text: `count=${count}` }],
          structuredContent: { count },
          isError,
        }),
    "exact_counter_journal_result",
  );
  return {
    state: response.state,
    resultHash: sha256(canonicalJson(parsed.data)),
    output: parsed.data.output,
    settlement: response.settlement,
  };
}
export async function delivered(
  ctx: Context,
  database: DatabaseObserver,
  scenario: StartedScenario,
) {
  const paths = await waitFor(
    ctx,
    "guest_proof_created",
    () =>
      proofPaths(
        ctx,
        scenario.client,
        scenario.sessionId,
        scenario.input.suffix,
      ),
    (paths) => paths.length > 0,
  );
  requireThat(paths.length <= 4, "bounded_cli_launches");
  const observed = await waitFor(
    ctx,
    "guest_result_received",
    async () => {
      const currentPaths = await proofPaths(
        ctx,
        scenario.client,
        scenario.sessionId,
        scenario.input.suffix,
      );
      requireThat(currentPaths.length <= 4, "bounded_cli_launches");
      const proofs = await Promise.all(
        currentPaths.map((path) =>
          readGuestProof(ctx, scenario.client, scenario.sessionId, path),
        ),
      );
      return proofs
        .filter((value) => value !== null)
        .filter(
          (value) =>
            value.proof.resultReceived &&
            value.proof.callId === scenario.input.callId &&
            value.proof.tool === scenario.alias,
        );
    },
    (values) => values.length >= 1,
  );
  const outbox = await waitFor(
    ctx,
    "actual_guest_ack",
    () => database.outbox(scenario.sessionId, scenario.input.callId),
    (value) => value.acknowledged,
  );
  requireThat(
    outbox.relayed && outbox.attempts >= 1,
    "outbox_relay_before_ack",
  );
  return { proofHashes: observed.map((value) => value.sha256), outbox };
}
export async function closeScenario(
  principals: Principals,
  scenario: StartedScenario,
) {
  await principals.admin.tasks.deleteTask({ taskId: scenario.taskId });
  await principals.admin.profiles.deleteProfile({ id: scenario.profileId });
  // Archived profiles retain grants. Keep the connection and journal for run-owned teardown.
}
