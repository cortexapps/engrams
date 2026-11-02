import { Code } from "@connectrpc/connect";
import { z } from "zod";
import {
  bindingTool,
  bindingTools,
  modelToolName,
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
  readGuestProofRecords,
} from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import {
  createAutomationTask,
  type CreatedScenarioTask,
} from "./automation.ts";
import type { CallPlan } from "./call-plan.ts";
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
    caller: "owner" | "unattended" | "automation";
    automationWrite: boolean;
    arguments: Record<string, unknown>;
    constraints: object[];
    callId: string;
    defaultAllow?: boolean;
    grant?: boolean;
    authKind?: "api_key" | "oauth";
    callPlan?: (
      alias: string,
      connection: { id: string; alias: string },
    ) => CallPlan;
    completionBarrier?: boolean;
    prematureBarrier?: boolean;
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
    input.authKind,
  );
  const profile = await createFixtureProfile(ctx, principals.admin, {
    ...input,
    connectionId: connection.id,
    connectionAlias: connection.alias,
    grant: input.grant ?? true,
    callPlan: input.callPlan?.(
      modelToolName(connection.id, connection.alias, input.upstreamName),
      { id: connection.id, alias: connection.alias },
    ),
    defaultAllow: input.defaultAllow ?? false,
  });
  const client = principals[input.caller];
  const reviewClient = input.caller !== "owner" ? principals.admin : client;
  const task: CreatedScenarioTask =
    input.caller === "automation"
      ? await createAutomationTask(
          ctx,
          principals,
          profile.profileId,
          input.suffix,
        )
      : await createHumanTask(ctx, client, profile.profileId, input.harness);
  await waitFor(
    ctx,
    "scenario_public_active",
    () => client.sessions.getSession({ sessionId: task.sessionId }),
    (value) =>
      value.session?.status === "active" && value.session.hostId === ctx.hostId,
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
  if (task.automation)
    requireThat(
      coordinator.source_key.creation.policy.actorId ===
        task.automation.runtimeActorId,
      "actual_automation_runtime_actor",
    );
  await waitFor(
    ctx,
    "orchestrator_started",
    () => database.managedLifecycle(task.sessionId, coordinator),
    (state) => state.phase === "started",
  );
  if (input.grant !== false) {
    const bound = await bindingTool(
      reviewClient,
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
  } else {
    requireThat(
      (await bindingTools(reviewClient, task.sessionId)).length === 0,
      "missing_grant_empty_binding",
    );
  }
  await guestReady(ctx, client, task.sessionId);
  return {
    ...task,
    ...profile,
    client,
    reviewClient,
    callerActorId: principals.actorIds[input.caller],
    reviewActorId:
      principals.actorIds[
        input.caller === "unattended" ? "admin" : input.caller
      ],
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
  marker?: string,
  promptAttempt = 1,
) {
  requireThat(
    Number.isInteger(promptAttempt) &&
      promptAttempt >= 1 &&
      promptAttempt <= 16,
    "prompt_attempt_bound",
  );
  requireThat(
    marker === undefined || /^[A-Za-z0-9_-]{1,64}$/.test(marker),
    "call_plan_marker",
  );
  await scenario.client.sessions.sendPrompt({
    sessionId: scenario.sessionId,
    text: marker ? `[[fixture:${marker}]]` : "Run the neutral fixture call.",
    promptId: `fixture-${scenario.input.suffix}${marker ? "-" + marker : ""}-${promptAttempt}`,
    ...(plan ? { harnessMode: "plan" } : {}),
  });
}
export async function execution(
  ctx: Context,
  scenario: StartedScenario,
  terminal = true,
  callId = scenario.input.callId,
) {
  return waitFor(
    ctx,
    "public_execution",
    async () => {
      const state = await scenario.reviewClient.integration.getSessionMcpState({
        sessionId: scenario.sessionId,
        omitBindings: true,
        pageSize: 16,
      });
      requireThat(!state.hasMoreExecutions, "fresh_session_execution_bound");
      const matches = state.executions.filter(
        (row) =>
          row.toolCallId === callId &&
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
      const response = await scenario.reviewClient.integration.listMcpApprovals(
        {
          sessionId: scenario.sessionId,
        },
      );
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
  const otherDenied = await expectRejected(
    () => principals.other.integration.decideMcpApproval(request),
    [Code.PermissionDenied, Code.NotFound],
  );
  const changedDenied = await expectRejected(
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
  const reversedDenied = await expectRejected(
    () =>
      principals[actor].integration.decideMcpApproval({
        ...request,
        approve: false,
      }),
    [Code.Aborted, Code.FailedPrecondition],
  );
  const replayDenied = await expectRejected(
    () => principals[actor].integration.decideMcpApproval(request),
    [Code.Aborted, Code.FailedPrecondition],
  );
  return {
    executionId: pending.executionId,
    contextHash: sha256(canonicalJson(pending.expectedReviewContext)),
    rejections: { otherDenied, changedDenied, reversedDenied, replayDenied },
  };
}
export async function verifiedCounterResult(
  scenario: StartedScenario,
  executionId: string,
  count: number,
  isError: boolean,
) {
  const response =
    await scenario.reviewClient.integration.getMcpExecutionResult({
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
export async function toolCompletionEvents(
  scenario: StartedScenario,
  callId = scenario.input.callId,
) {
  const page = await scenario.reviewClient.sessions.listSessionEvents({
    sessionId: scenario.sessionId,
    kinds: ["tool_call_completed"],
    toolNames: [scenario.alias],
    limit: 129n,
  });
  requireThat(page.events.length < 129, "completion_event_bound");
  return page.events.flatMap((event) => {
    requireThat(
      event.kind === "tool_call_completed" && event.idx !== undefined,
      "completion_event_identity",
    );
    const parsed = z
      .object({
        tool_call_id: z.string(),
        tool_name: z.string(),
        run_id: z.string(),
        ok: z.boolean(),
      })
      .safeParse(JSON.parse(event.payloadJson));
    requireThat(
      parsed.success && parsed.data.tool_name === scenario.alias,
      "completion_event_shape",
    );
    return parsed.data.tool_call_id === callId
      ? [
          {
            ...parsed.data,
            idx: String(event.idx),
            sha256: sha256(event.payloadJson),
          },
        ]
      : [];
  });
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
          readGuestProofRecords(ctx, scenario.client, scenario.sessionId, path),
        ),
      );
      return proofs
        .flat()
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
  const completion = await waitFor(
    ctx,
    "actual_native_completion_event",
    () => toolCompletionEvents(scenario),
    (events) => events.length > 0,
  );
  const target = await database.deliveryTarget(
    scenario.sessionId,
    scenario.input.callId,
  );
  requireThat(!target.targetGone, "guest_ack_is_not_target_gone_settlement");
  return {
    proofHashes: observed.map((value) => value.sha256),
    outbox,
    completion,
    target,
  };
}

/** A native receipt is separate from outbox ACK and upstream outcome. */
type ProofSelection = {
  callId: string;
  tool: string;
  planSha256?: string;
  afterCount?: number;
};
export async function pollProof(
  ctx: Context,
  scenario: StartedScenario,
  input: ProofSelection,
) {
  return pollSelectedProof(ctx, scenario, input);
}
export async function pollGuestObservation(
  ctx: Context,
  scenario: StartedScenario,
  input: ProofSelection & {
    observation:
      | "completionBarrierHeld"
      | "completionBarrierReleased"
      | "prematureBarrierHeld"
      | "prematureBarrierReleased"
      | "prematureCompletionEmitted";
  },
) {
  return pollSelectedProof(ctx, scenario, input);
}
async function pollSelectedProof(
  ctx: Context,
  scenario: StartedScenario,
  input: {
    callId: string;
    tool: string;
    planSha256?: string;
    afterCount?: number;
    observation?:
      | "completionBarrierHeld"
      | "completionBarrierReleased"
      | "prematureBarrierHeld"
      | "prematureBarrierReleased"
      | "prematureCompletionEmitted";
  },
) {
  requireThat(
    /^[A-Za-z0-9_-]{1,64}$/.test(input.callId) &&
      /^[A-Za-z0-9_]{1,128}$/.test(input.tool),
    "proof_selection_identity",
  );
  const afterCount = input.afterCount ?? 0;
  requireThat(
    Number.isInteger(afterCount) && afterCount >= 0 && afterCount <= 128,
    "proof_selection_count",
  );
  return waitFor(
    ctx,
    "native_completed_proof",
    async () => {
      const paths = await proofPaths(
        ctx,
        scenario.client,
        scenario.sessionId,
        scenario.input.suffix,
      );
      requireThat(paths.length <= 16, "proof_launch_bound");
      const records = (
        await Promise.all(
          paths.map((path) =>
            readGuestProofRecords(
              ctx,
              scenario.client,
              scenario.sessionId,
              path,
            ),
          ),
        )
      ).flat();
      return records.filter(
        (record) =>
          record.proof.callId === input.callId &&
          record.proof.tool === input.tool &&
          (input.planSha256 === undefined ||
            record.proof.planSha256 === input.planSha256) &&
          (input.observation !== undefined
            ? record.proof.observation === input.observation
            : record.proof.observation === undefined &&
              (record.proof.resultReceived === true ||
                record.proof.forgedRejected === true ||
                (record.proof.expectExposure === "absent" &&
                  record.proof.requestAttempted === true &&
                  record.proof.protocolDenialCode === "unknown_tool"))),
      );
    },
    (records) => records.length > afterCount,
  );
}
export async function closeScenario(
  principals: Principals,
  scenario: StartedScenario,
) {
  if (scenario.automation) {
    const run = await principals.automation.automationRuns.getRun({
      runId: scenario.automation.runId,
    });
    if (
      run.run?.brief &&
      ![
        "completed",
        "failed",
        "filtered",
        "superseded",
        "halted",
        "deadline",
      ].includes(run.run.brief.status)
    )
      await principals.automation.automationRuns.stopRun({
        runId: scenario.automation.runId,
        reason: "Fixture cleanup",
      });
    await principals.admin.automations.archiveAutomation({
      id: scenario.automation.id,
    });
  }
  await principals.admin.tasks.deleteTask({ taskId: scenario.taskId });
  await principals.admin.profiles.deleteProfile({ id: scenario.profileId });
  // Archived profiles retain grants. Keep the connection and journal for run-owned teardown.
}
