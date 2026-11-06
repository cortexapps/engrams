import { Code } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import { z } from "zod";
import { ProfileIntegrationGrantSchema } from "../../src/gen/engram/app/v1/profile_pb.ts";
import {
  bindingTools,
  expectRejected,
  modelToolName,
  waitFor,
} from "./bootstrap.ts";
import { grantUpdateRequest } from "./rebind.ts";
export { grantUpdateRequest } from "./rebind.ts";
import type { CallPlan } from "./call-plan.ts";
import { requireThat, RunnerFailure, sha256 } from "./context.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverAssertion,
  type DriverObservation,
} from "./driver.ts";
import { canonicalJson, releaseGuestBarrier } from "./guest.ts";
import {
  approveOnce,
  closeScenario,
  execution,
  pollProof,
  pollGuestObservation,
  sendScenarioPrompt,
  startScenario,
  scenarioBindingTools,
  verifiedCounterResult,
  type StartedScenario,
} from "./scenario-runtime.ts";

export function assertReadOnlyPolicy(
  policy: { writeEligibility: string } | undefined,
) {
  requireThat(
    policy?.writeEligibility === "read_only",
    "generic_plan_preserves_observed_read_only_policy",
  );
}
export function assertPolicyDenial(proof: {
  resultReceived?: boolean;
  delivery?: string;
  outcome?: string | null;
  isError?: boolean | null;
}) {
  requireThat(
    proof.resultReceived === true &&
      proof.delivery === "available" &&
      proof.outcome === null &&
      proof.isError === true,
    "actual_runtime_policy_refusal",
  );
}
export function assertNoDispatch(
  before: { reads: number; effects: { count: number; executions: number } },
  after: typeof before,
) {
  requireThat(
    after.reads === before.reads &&
      after.effects.count === before.effects.count &&
      after.effects.executions === before.effects.executions,
    "no_upstream_dispatch_or_effect",
  );
}
export function assertAbsentToolDenial(
  harness: "claude" | "codex",
  proof: {
    expectExposure?: string;
    requestAttempted?: boolean;
    protocolDenialCode?: string;
    nativeDenialCode?: number;
    exposedMcpToolNamesSha256?: string;
    resultReceived?: boolean;
    selectedSchemaSha256?: string;
  },
) {
  requireThat(
    proof.expectExposure === "absent" &&
      proof.requestAttempted === true &&
      proof.protocolDenialCode === "unknown_tool" &&
      (harness !== "claude" || proof.nativeDenialCode === -32602) &&
      !!proof.exposedMcpToolNamesSha256 &&
      proof.resultReceived === undefined &&
      proof.selectedSchemaSha256 === undefined,
    "actual_absent_tool_native_refusal",
  );
}
export function assertNoProjectedAuthority(
  call: {
    state: string;
    approvalDecision: string;
    executionId?: string;
    cancellationReceipt?: { executionId: string; state: string };
  },
  resultJson: string | undefined,
) {
  requireThat(
    (call.state === "waiting_approval" &&
      call.approvalDecision === "pending") ||
      (call.state === "canceled" &&
        call.approvalDecision === "denied" &&
        !!call.executionId &&
        call.cancellationReceipt?.executionId === call.executionId &&
        call.cancellationReceipt.state === "canceled"),
    `guest_projection_has_no_journal_authority_${projectionState(call)}`,
  );
  if (resultJson !== undefined) {
    const result = z
      .object({ outcome: z.string() })
      .parse(JSON.parse(resultJson));
    requireThat(
      result.outcome !== "succeeded",
      "guest_projection_has_no_success_result",
    );
  }
}
function projectionState(
  call: { state: string; approvalDecision: string } | undefined,
) {
  const state = [
    "waiting_approval",
    "authorized",
    "dispatching",
    "succeeded",
    "denied",
    "failed",
    "canceled",
    "outcome_unknown",
  ].includes(call?.state ?? "")
    ? call!.state
    : "other";
  const decision = ["", "pending", "approved", "denied"].includes(
    call?.approvalDecision ?? "",
  )
    ? call?.approvalDecision || "empty"
    : "other";
  return `${state}_${decision}`;
}
async function absentCall(
  input: Parameters<CaseDriver>[0],
  scenario: StartedScenario,
  marker: string,
  callId: string,
  tool: string,
) {
  const before = await scenario.fixture.observe();
  const tools = await scenarioBindingTools(scenario);
  requireThat(
    !tools.some((value) => value.modelToolName === tool),
    "absent_alias_public_binding",
  );
  await sendScenarioPrompt(scenario, false, marker);
  const proofs = await pollProof(input.ctx, scenario, {
    callId,
    tool,
    planSha256: scenario.planSha256,
  });
  requireThat(proofs.length === 1, "single_absent_call_proof");
  assertAbsentToolDenial(input.harness, proofs[0]!.proof);
  requireThat(
    proofs[0]!.proof.exposedMcpToolNamesSha256 ===
      `sha256:${sha256(canonicalJson(tools.map((value) => value.modelToolName).sort()))}`,
    "absent_call_actual_exposure_matches_binding",
  );
  const after = await scenario.fixture.observe();
  assertNoDispatch(before, after);
  return [
    policyObservation("guest-proof", scenario.sessionId, "denied", {
      hash: proofs[0]!.sha256,
    }),
    policyObservation(
      "fixture-observer",
      scenario.sessionId,
      "denied",
      { before, after },
      0,
    ),
  ];
}

export function fixedPolicyPlan(
  alias: string,
  kind: "read" | "write",
): CallPlan {
  return {
    steps: [
      {
        marker: "allowed",
        tool: alias,
        arguments: kind === "read" ? {} : { delta: 1 },
        callId: "policy-allowed",
        resultFormat: "mcp",
      },
      {
        marker: "restricted",
        tool: alias,
        arguments: { delta: 2 },
        callId: "policy-restricted",
        resultFormat: "mcp",
      },
      {
        marker: "after-plan",
        tool: alias,
        arguments: kind === "read" ? {} : { delta: 1 },
        callId: "policy-after-plan",
        resultFormat: "mcp",
      },
      {
        marker: "plan",
        tool: "exit_plan_mode",
        arguments: { plan: "Run the neutral counter call." },
        callId: "policy-plan",
        resultFormat: "json",
      },
      {
        marker: "spawn",
        tool: "spawn_session",
        arguments: {
          task_name: "fixture-child",
          message: "[[fixture:child]]",
          idempotency_key: "fixture-child",
        },
        callId: "policy-spawn",
        resultFormat: "json",
      },
      {
        marker: "child",
        tool: alias,
        arguments: kind === "read" ? {} : { delta: 1 },
        callId: "policy-child",
        resultFormat: "mcp",
      },
    ],
  };
}
export function expectedPlanApproval(harness: "claude" | "codex") {
  const text = '{"decision":"approve"}';
  return harness === "claude"
    ? { content: [{ type: "text", text }] }
    : { success: true, contentItems: [{ type: "inputText", text }] };
}
export function policyObservation(
  source: DriverObservation["source"],
  identity: string,
  state: DriverObservation["state"],
  metadata: unknown,
  count?: number,
): DriverObservation {
  return {
    source,
    identitySha256: sha256(identity),
    state,
    contentSha256: sha256(canonicalJson(metadata)),
    ...(count === undefined ? {} : { count }),
  };
}
export const deltaOne = {
  type: "object",
  properties: { delta: { const: 1 } },
  required: ["delta"],
  additionalProperties: false,
};
export function assertDeltaOneConstraints(documents: string[]) {
  requireThat(
    documents.length > 0 &&
      documents.every(
        (document) =>
          canonicalJson(JSON.parse(document)) === canonicalJson(deltaOne),
      ),
    "exact_delta_one_constraint_roots",
  );
}
export async function deniedCall(
  input: Parameters<CaseDriver>[0],
  scenario: StartedScenario,
  marker: string,
  callId: string,
) {
  const before = await scenario.fixture.observe();
  await sendScenarioPrompt(scenario, false, marker);
  const proofs = await pollProof(input.ctx, scenario, {
    callId,
    tool: scenario.alias,
    planSha256: scenario.planSha256,
  });
  requireThat(proofs.length === 1, "single_denied_guest_completion");
  assertPolicyDenial(proofs[0]!.proof);
  const after = await scenario.fixture.observe();
  assertNoDispatch(before, after);
  return [
    policyObservation("guest-proof", scenario.sessionId, "denied", {
      hash: proofs[0]!.sha256,
      callId,
    }),
    policyObservation(
      "fixture-observer",
      scenario.sessionId,
      "denied",
      { before, after },
      0,
    ),
  ];
}

export const unattendedGrantDriver: CaseDriver = async (input) => {
  const { ctx, principals, database, harness } = input;
  const actor = await principals.automation.identity.whoAmI({});
  const human = await principals.admin.identity.whoAmI({});
  requireThat(
    actor.serviceAccount &&
      !human.serviceAccount &&
      actor.userId !== human.userId,
    "distinct_unattended_and_review_actors",
  );
  const actors = [
    policyObservation("public-rpc", actor.userId, "created", {
      actor: sha256(actor.userId),
      reviewer: sha256(human.userId),
      serviceAccount: actor.serviceAccount,
    }),
  ];
  const assertions = [{ assertion: "api-key-principal", observations: actors }];
  for (const automationWrite of [false, true]) {
    const scenario = await startScenario(ctx, principals, database, {
      harness,
      suffix: `${harness}-unattended-${automationWrite ? "allow" : "deny"}`,
      protocol: "modern",
      upstreamName: "write_counter",
      mode: "write",
      caller: "automation",
      automationWrite,
      arguments: { delta: 1 },
      constraints: [deltaOne],
      callId: "policy-allowed",
      callPlan: (alias) => fixedPolicyPlan(alias, "write"),
    });
    try {
      requireThat(
        scenario.automation &&
          scenario.automation.runtimeActorId ===
            `automation:${scenario.automation.id}` &&
          scenario.automation.triggerActorId === actor.userId &&
          scenario.automation.definitionActorId === human.userId &&
          scenario.coordinator.source_key.creation.policy.actorId ===
            scenario.automation.runtimeActorId,
        "authoritative_automation_runtime_and_trigger_actors",
      );
      const forbiddenTrigger = await expectRejected(
        () =>
          principals.unattended.automations.runNow({
            automationId: scenario.automation!.id,
          }),
        [Code.PermissionDenied],
      );
      actors.push(
        policyObservation(
          "public-rpc",
          scenario.automation.runtimeActorId,
          "created",
          { automation: scenario.automation, forbiddenTrigger },
        ),
      );
      const tools = await bindingTools(principals.admin, scenario.sessionId);
      requireThat(
        tools.length === 1 &&
          tools[0]!.automationWrite === automationWrite &&
          !tools[0]!.unrestricted,
        "exact_admin_automation_grant",
      );
      assertDeltaOneConstraints(tools[0]!.argumentConstraintDocumentsJson);
      if (!automationWrite) {
        assertions.push({
          assertion: "default-deny",
          observations: await deniedCall(
            input,
            scenario,
            "allowed",
            "policy-allowed",
          ),
        });
        // The generic approval path returns a native decision; it is not MCP authority.
        await sendScenarioPrompt(scenario, true, "plan");
        const completed = await pollProof(ctx, scenario, {
          callId: "policy-plan",
          tool: "exit_plan_mode",
          planSha256: scenario.planSha256,
        });
        requireThat(
          completed.length === 1 &&
            completed[0]!.proof.resultReceived &&
            completed[0]!.proof.resultSha256 ===
              `sha256:${sha256(canonicalJson(expectedPlanApproval(harness)))}`,
          "actual_generic_plan_completion",
        );
        const afterPlan = await principals.admin.sessions.getMcpWritePolicy({
          sessionId: scenario.sessionId,
        });
        assertReadOnlyPolicy(afterPlan.policy);
        assertions.push({
          assertion: "plan-approval-insufficient",
          observations: await deniedCall(
            input,
            scenario,
            "after-plan",
            "policy-after-plan",
          ),
        });
      } else {
        const before = await scenario.fixture.observe();
        await sendScenarioPrompt(scenario, false, "allowed");
        const call = await execution(ctx, scenario);
        requireThat(
          call?.state === "succeeded" && !call.approvalDecision,
          "unattended_write_without_human_approval",
        );
        const result = await verifiedCounterResult(
          scenario,
          call.executionId,
          before.effects.count + 1,
          false,
        );
        const proofs = await pollProof(ctx, scenario, {
          callId: "policy-allowed",
          tool: scenario.alias,
          planSha256: scenario.planSha256,
        });
        requireThat(
          proofs.length === 1 &&
            proofs[0]!.proof.outcome === "succeeded" &&
            proofs[0]!.proof.delivery === "available",
          "unattended_actual_result",
        );
        const after = await scenario.fixture.observe();
        requireThat(
          after.effects.executions === before.effects.executions + 1 &&
            after.effects.count === before.effects.count + 1,
          "unattended_one_effect",
        );
        assertions.push({
          assertion: "explicit-admin-grant",
          observations: [
            policyObservation("public-rpc", scenario.sessionId, "succeeded", {
              resultHash: result.resultHash,
              grant: tools[0],
            }),
            policyObservation(
              "fixture-observer",
              scenario.sessionId,
              "succeeded",
              { before, after },
              1,
            ),
          ],
        });
        assertions.push({
          assertion: "argument-constraint",
          observations: await deniedCall(
            input,
            scenario,
            "restricted",
            "policy-restricted",
          ),
        });
      }
    } finally {
      await closeScenario(principals, scenario);
    }
  }
  return completeAssertions("unattended-grant", assertions);
};

export const grantDenialsDriver: CaseDriver = async (input) => {
  const { ctx, principals, database, harness } = input;
  const assertions: DriverAssertion[] = [];
  const noEffects: DriverObservation[] = [];
  const missing = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-grant-missing`,
    protocol: "modern",
    upstreamName: "read_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: {},
    constraints: [],
    callId: "grant-missing",
    grant: false,
    callPlan: (alias) => ({
      steps: [
        {
          marker: "missing",
          tool: alias,
          arguments: {},
          callId: "grant-missing",
          resultFormat: "mcp",
          expectExposure: "absent",
        },
      ],
    }),
  });
  try {
    const current = await principals.admin.profiles.getProfile({
      id: missing.profileId,
    });
    requireThat(
      current.profile && current.profile.integrationGrants.length === 0,
      "missing_profile_grant",
    );
    const forgedGrant = create(ProfileIntegrationGrantSchema, {
      connectionId: missing.connection.id,
      operation: "invoke",
      credentialScope: "org",
      mcp: {
        upstreamName: "write_counter",
        unrestricted: true,
        automationWrite: true,
      },
    });
    const refusal = await expectRejected(
      () =>
        principals.other.profiles.updateProfile(
          grantUpdateRequest(current.profile!, [forgedGrant]),
        ),
      [Code.PermissionDenied],
    );
    const unchanged = await principals.admin.profiles.getProfile({
      id: missing.profileId,
    });
    requireThat(
      unchanged.profile?.mcpGrantRevision ===
        current.profile.mcpGrantRevision &&
        unchanged.profile.integrationGrants.length === 0,
      "forged_grant_not_persisted",
    );
    assertions.push({
      assertion: "forged-grant",
      observations: [
        policyObservation("public-rpc", missing.profileId, "denied", {
          refusal,
          revision: unchanged.profile.mcpGrantRevision.toString(),
        }),
      ],
    });
    const observations = await absentCall(
      input,
      missing,
      "missing",
      "grant-missing",
      missing.alias,
    );
    assertions.push({ assertion: "missing-grant", observations });
    noEffects.push(
      ...observations.filter((value) => value.source === "fixture-observer"),
    );
  } finally {
    await closeScenario(principals, missing);
  }
  const revoked = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-grant-revoked`,
    protocol: "modern",
    upstreamName: "read_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: {},
    constraints: [],
    callId: "policy-allowed",
    callPlan: (alias) => fixedPolicyPlan(alias, "read"),
  });
  try {
    const current = await principals.admin.profiles.getProfile({
      id: revoked.profileId,
    });
    requireThat(current.profile, "current_profile_before_revoke");
    const updated = await principals.admin.profiles.updateProfile(
      grantUpdateRequest(current.profile, []),
    );
    requireThat(
      updated.profile?.mcpGrantRevision ===
        current.profile.mcpGrantRevision + 1n &&
        updated.profile.integrationGrants.length === 0,
      "admin_revoked_exact_grant",
    );
    const observations = await deniedCall(
      input,
      revoked,
      "allowed",
      "policy-allowed",
    );
    assertions.push({ assertion: "revoked-grant", observations });
    noEffects.push(
      ...observations.filter((value) => value.source === "fixture-observer"),
    );
  } finally {
    await closeScenario(principals, revoked);
  }
  const forged = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-guest-projection`,
    protocol: "modern",
    upstreamName: "write_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: { delta: 1 },
    constraints: [deltaOne],
    callId: "grant-projection",
    prematureBarrier: true,
    callPlan: (alias) => ({
      steps: [
        {
          marker: "projection",
          tool: alias,
          arguments: { delta: 1 },
          callId: "grant-projection",
          resultFormat: "mcp",
          completionProjection: "premature",
        },
      ],
    }),
  });
  try {
    const before = await forged.fixture.observe();
    await sendScenarioPrompt(forged, false, "projection");
    const held = await pollGuestObservation(ctx, forged, {
      callId: "grant-projection",
      tool: forged.alias,
      planSha256: forged.planSha256,
      observation: "prematureBarrierHeld",
    });
    requireThat(held.length === 1, "single_premature_barrier");
    let lastPending: Awaited<ReturnType<typeof execution>>;
    const pending = await waitFor(
      ctx,
      "real_pending_call_before_guest_projection",
      async () => (lastPending = await execution(ctx, forged, false)),
      (call) =>
        call?.state === "waiting_approval" &&
        call.approvalDecision === "pending" &&
        !!call.runId,
    ).catch((error: unknown) => {
      if (
        !(error instanceof RunnerFailure) ||
        error.seam !== "real_pending_call_before_guest_projection"
      )
        throw error;
      requireThat(
        false,
        `real_pending_call_before_guest_projection_${projectionState(lastPending)}`,
      );
    });
    requireThat(
      pending?.state === "waiting_approval" &&
        pending.approvalDecision === "pending" &&
        pending.runId,
      "real_pending_call_before_guest_projection",
    );
    assertNoDispatch(before, await forged.fixture.observe());
    await releaseGuestBarrier(
      ctx,
      forged.client,
      forged.sessionId,
      "premature",
    );
    const emitted = await pollGuestObservation(ctx, forged, {
      callId: "grant-projection",
      tool: forged.alias,
      planSha256: forged.planSha256,
      observation: "prematureCompletionEmitted",
    });
    requireThat(
      emitted.length === 1 && emitted[0]!.proof.eventSha256,
      "actual_guest_completion_emission",
    );
    const completedEvent = z.object({
      run_id: z.string(),
      tool_call_id: z.string(),
      tool_name: z.string(),
      ok: z.boolean(),
    });
    const runEvent = z.object({ run_id: z.string() });
    const transported = await waitFor(
      ctx,
      "actual_transported_guest_completion",
      async () => {
        const page = await forged.reviewClient.sessions.listSessionEvents({
          sessionId: forged.sessionId,
          limit: 16n,
          kinds: ["tool_call_completed", "run_completed", "run_interrupted"],
        });
        requireThat(page.events.length < 16, "fresh_projection_event_bound");
        const completions = page.events
          .filter((event) => event.kind === "tool_call_completed")
          .map((event) => ({
            idx: event.idx,
            value: completedEvent.parse(JSON.parse(event.payloadJson)),
          }))
          .filter(
            (event) =>
              event.value.tool_call_id === pending.toolCallId &&
              event.value.run_id === pending.runId,
          );
        requireThat(completions.length <= 1, "single_transported_projection");
        const completion = completions[0];
        const finished = page.events.find(
          (event) =>
            event.kind !== "tool_call_completed" &&
            runEvent.parse(JSON.parse(event.payloadJson)).run_id ===
              pending.runId,
        );
        if (!completion || !finished) return undefined;
        requireThat(
          completion.idx !== undefined &&
            finished.idx !== undefined &&
            finished.idx > completion.idx &&
            completion.value.ok &&
            completion.value.tool_name ===
              (harness === "claude"
                ? `mcp__engrams__${forged.alias}`
                : forged.alias),
          "native_projection_matches_real_call",
        );
        return {
          completion: completion.value,
          completionIdx: completion.idx.toString(),
          finishedIdx: finished.idx.toString(),
        };
      },
      (value) => !!value,
    );
    const afterCall = await execution(ctx, forged, false);
    requireThat(
      afterCall && afterCall.executionId === pending.executionId,
      "same_pending_journal_identity",
    );
    const result = await forged.reviewClient.integration.getMcpExecutionResult({
      sessionId: forged.sessionId,
      executionId: pending.executionId,
    });
    assertNoProjectedAuthority(afterCall, result.resultJson);
    requireThat(
      result.state === afterCall.state,
      "projection_result_state_matches_journal",
    );
    const after = await forged.fixture.observe();
    assertNoDispatch(before, after);
    const effects = policyObservation(
      "fixture-observer",
      forged.sessionId,
      "denied",
      { before, after },
      0,
    );
    noEffects.push(effects);
    assertions.push({
      assertion: "forged-guest-completion",
      observations: [
        policyObservation("guest-proof", forged.sessionId, "denied", {
          emission: emitted[0]!.sha256,
        }),
        policyObservation("public-rpc", pending.executionId, "denied", {
          transported,
          state: afterCall.state,
          approvalDecision: afterCall.approvalDecision,
          resultHash: result.resultJson ? sha256(result.resultJson) : null,
        }),
        effects,
      ],
    });
  } finally {
    await closeScenario(principals, forged);
  }
  assertions.push({ assertion: "no-effect", observations: noEffects });
  return completeAssertions("grant-denials", assertions);
};

export const planChildIntersectionDriver: CaseDriver = async (input) => {
  const { ctx, principals, database, harness } = input;
  const assertions: DriverAssertion[] = [];
  for (const upstreamName of ["read_counter", "write_counter"] as const) {
    const scenario = await startScenario(ctx, principals, database, {
      harness,
      suffix: `${harness}-plan-${upstreamName === "read_counter" ? "read" : "write"}`,
      protocol: "modern",
      upstreamName,
      mode: upstreamName === "read_counter" ? "read" : "write",
      caller: "owner",
      automationWrite: false,
      arguments: upstreamName === "read_counter" ? {} : { delta: 1 },
      constraints: upstreamName === "read_counter" ? [] : [deltaOne],
      callId: "policy-allowed",
      callPlan: (alias) =>
        fixedPolicyPlan(
          alias,
          upstreamName === "read_counter" ? "read" : "write",
        ),
    });
    try {
      const before = await scenario.fixture.observe();
      await sendScenarioPrompt(scenario, true, "allowed");
      const proofs = await pollProof(ctx, scenario, {
        callId: "policy-allowed",
        tool: scenario.alias,
        planSha256: scenario.planSha256,
      });
      requireThat(proofs.length === 1, "one_plan_call_proof");
      const after = await scenario.fixture.observe();
      if (upstreamName === "read_counter") {
        const call = await execution(ctx, scenario);
        requireThat(
          call?.state === "succeeded" &&
            after.reads === before.reads + 1 &&
            after.effects.executions === before.effects.executions,
          "plan_read_succeeds",
        );
        await verifiedCounterResult(
          scenario,
          call.executionId,
          before.effects.count,
          false,
        );
      } else {
        assertPolicyDenial(proofs[0]!.proof);
        requireThat(
          proofs[0]!.proof.denialPlanMode,
          "trusted_plan_write_denial",
        );
        assertNoDispatch(before, after);
      }
      assertions.push({
        assertion:
          upstreamName === "read_counter"
            ? "plan-read-only"
            : "plan-write-denied",
        observations: [
          policyObservation(
            "guest-proof",
            scenario.sessionId,
            upstreamName === "read_counter" ? "succeeded" : "denied",
            { proof: proofs[0]!.sha256 },
          ),
          policyObservation(
            "fixture-observer",
            scenario.sessionId,
            upstreamName === "read_counter" ? "succeeded" : "denied",
            { before, after },
            0,
          ),
        ],
      });
    } finally {
      await closeScenario(principals, scenario);
    }
  }
  let outsideAlias = "";
  const parent = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-child`,
    protocol: "modern",
    upstreamName: "write_counter",
    mode: "write",
    caller: "owner",
    automationWrite: false,
    arguments: { delta: 1 },
    constraints: [deltaOne],
    callId: "policy-spawn",
    callPlan: (alias, connection) => {
      outsideAlias = modelToolName(
        connection.id,
        connection.alias,
        "read_counter",
      );
      return {
        steps: [
          ...fixedPolicyPlan(alias, "write").steps,
          {
            marker: "outside",
            tool: outsideAlias,
            arguments: {},
            callId: "policy-outside",
            resultFormat: "mcp",
            expectExposure: "absent",
          },
        ],
      };
    },
  });
  let child: StartedScenario | undefined;
  try {
    const parentTools = await bindingTools(principals.owner, parent.sessionId);
    await sendScenarioPrompt(parent, false, "spawn");
    const spawned = await waitFor(
      ctx,
      "real_native_child_task",
      async () => {
        const value = await principals.owner.tasks.getTask({
          taskId: parent.taskId,
        });
        const matches =
          value.task?.descendants.filter(
            (task) =>
              task.localTaskName === "fixture-child" &&
              task.parentTaskId === parent.taskId &&
              task.spawningSessionId === parent.sessionId,
          ) ?? [];
        requireThat(matches.length <= 1, "single_real_native_child");
        return matches[0];
      },
      (task) => !!task && task.sessions.length === 1,
    );
    requireThat(
      spawned && spawned.sessions.length === 1,
      "native_child_identity",
    );
    child = {
      ...parent,
      taskId: spawned.id,
      sessionId: spawned.sessions[0]!.sessionId,
      input: { ...parent.input, callId: "policy-child" },
    };
    database.own(child.taskId, child.sessionId);
    const started = await waitFor(
      ctx,
      "child_actual_started",
      () => database.started(child!.sessionId, ctx.hostId),
      (value) => value.phase === "started",
    );
    requireThat(
      started.source_key.creation.policy.parentSessionId === parent.sessionId,
      "child_frozen_parent_source_identity",
    );
    await waitFor(
      ctx,
      "child_managed_started",
      () => database.managedLifecycle(child!.sessionId, started),
      (value) => value.phase === "started",
    );
    const childTools = await bindingTools(principals.owner, child.sessionId);
    requireThat(
      parentTools.length === 1 && childTools.length === 1,
      "single_parent_and_child_tool",
    );
    const {
      argumentConstraintDocumentsJson: parentConstraints,
      ...parentIdentity
    } = parentTools[0]!;
    const {
      argumentConstraintDocumentsJson: childConstraints,
      ...childIdentity
    } = childTools[0]!;
    assertDeltaOneConstraints(parentConstraints);
    assertDeltaOneConstraints(childConstraints);
    // Intersections preserve each schema root, including repeated equivalent restrictions.
    requireThat(
      canonicalJson(childIdentity) === canonicalJson(parentIdentity) &&
        !childIdentity.unrestricted &&
        childConstraints.length >= parentConstraints.length,
      "native_child_parent_derived_grant_intersection",
    );
    const before = await parent.fixture.observe();
    await approveOnce(ctx, principals, child, "owner");
    const completed = await execution(ctx, child);
    requireThat(
      completed?.state === "succeeded",
      "allowed_child_write_succeeds",
    );
    await verifiedCounterResult(
      child,
      completed.executionId,
      before.effects.count + 1,
      false,
    );
    await pollProof(ctx, child, {
      callId: "policy-child",
      tool: child.alias,
      planSha256: child.planSha256,
    });
    const spawnProof = await pollProof(ctx, parent, {
      callId: "policy-spawn",
      tool: "spawn_session",
      planSha256: parent.planSha256,
    });
    requireThat(
      spawnProof.length === 1 && spawnProof[0]!.proof.resultSha256,
      "actual_native_spawn_result",
    );
    const after = await parent.fixture.observe();
    requireThat(
      after.effects.executions === before.effects.executions + 1 &&
        after.effects.count === before.effects.count + 1,
      "allowed_child_exactly_one_effect",
    );
    const absent = await absentCall(
      input,
      child,
      "outside",
      "policy-outside",
      outsideAlias,
    );
    assertions.push({
      assertion: "child-grant-intersection",
      observations: [
        policyObservation("public-rpc", child.sessionId, "succeeded", {
          parentTools,
          childTools,
          parentSessionHash: sha256(parent.sessionId),
          spawnProof: spawnProof[0]!.sha256,
        }),
        ...absent,
      ],
    });
    const denied = await deniedCall(
      input,
      child,
      "restricted",
      "policy-restricted",
    );
    assertions.push({
      assertion: "child-argument-intersection",
      observations: [
        policyObservation(
          "fixture-observer",
          child.sessionId,
          "succeeded",
          { before, after },
          1,
        ),
        ...denied,
      ],
    });
  } finally {
    try {
      if (child)
        await principals.admin.tasks.deleteTask({ taskId: child.taskId });
    } finally {
      await closeScenario(principals, parent);
    }
  }
  return completeAssertions("plan-child-intersection", assertions);
};
