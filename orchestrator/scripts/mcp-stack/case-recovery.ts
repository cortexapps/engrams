import { requireThat, sha256 } from "./context.ts";
import { waitFor } from "./bootstrap.ts";
import {
  canonicalJson,
  proofPaths,
  readGuestProofRecords,
  expectedCounterNative,
  releaseGuestBarrier,
} from "./guest.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverObservation,
} from "./driver.ts";
import { FixtureObserver } from "./observer.ts";
import { Supervisor } from "./supervisor.ts";
import {
  delivered,
  pollGuestObservation,
  pollProof,
  toolCompletionEvents,
  verifiedCounterResult,
  closeScenario,
  execution,
  sendScenarioPrompt,
  startScenario,
  type StartedScenario,
} from "./scenario-runtime.ts";
import { recoverCurrentBinding, revokeProfileGrants } from "./rebind.ts";
import { McpCompletionSettlement } from "../../src/gen/engram/app/v1/integration_pb.ts";
import type { Context } from "./context.ts";

async function completions(
  ctx: Context,
  scenario: StartedScenario,
  callId: string,
) {
  const paths = await proofPaths(
    ctx,
    scenario.client,
    scenario.sessionId,
    scenario.input.suffix,
  );
  requireThat(paths.length <= 16, "recovery_proof_launch_bound");
  const records = (
    await Promise.all(
      paths.map((path) =>
        readGuestProofRecords(ctx, scenario.client, scenario.sessionId, path),
      ),
    )
  ).flat();
  return records.filter(
    (record) =>
      record.proof.callId === callId &&
      record.proof.tool === scenario.alias &&
      record.proof.planSha256 === scenario.planSha256 &&
      record.proof.resultReceived,
  );
}

export function assertNewIdWriteDenial(
  denied: Awaited<ReturnType<typeof completions>>,
) {
  requireThat(
    denied.length > 0 &&
      denied.every(
        ({ proof }) =>
          proof.resultReceived === true &&
          proof.delivery === "available" &&
          proof.outcome === null &&
          proof.isError === true,
      ),
    "new_id_actual_runtime_policy_refusal",
  );
}

export function assertDuplicateWriteUnknown(
  repeated: Awaited<ReturnType<typeof completions>>,
) {
  requireThat(
    repeated.length > 0 &&
      repeated.every(
        ({ proof }) =>
          proof.outcome === "outcome_unknown" &&
          proof.delivery === "unavailable",
      ),
    "duplicate_id_stays_unknown",
  );
}

export const heldWriteRestartDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const fixture = new FixtureObserver(ctx);
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: true,
    revision: 1,
  });
  const before = await fixture.observe();
  const scenario = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-held-restart`,
    protocol: "modern",
    upstreamName: "write_counter",
    mode: "write",
    caller: "automation",
    automationWrite: true,
    arguments: { delta: 1 },
    constraints: [],
    callId: "held-write-1",
    callPlan: (alias) => ({
      steps: [
        {
          marker: "first",
          tool: alias,
          arguments: { delta: 1 },
          callId: "held-write-1",
          resultFormat: "mcp",
        },
        {
          marker: "second",
          tool: alias,
          arguments: { delta: 1 },
          callId: "held-write-2",
          resultFormat: "mcp",
        },
      ],
    }),
  });
  const identitySha256 = sha256(
    canonicalJson({
      task: scenario.taskId,
      session: scenario.sessionId,
      call: scenario.input.callId,
    }),
  );
  const observations = (
    source: DriverObservation["source"],
    state: DriverObservation["state"],
    value: unknown,
  ): DriverObservation[] => [
    {
      source,
      state,
      identitySha256,
      contentSha256: sha256(canonicalJson(value)),
      ...(source === "public-rpc"
        ? { actorSha256: sha256(scenario.reviewActorId) }
        : {}),
    },
  ];
  try {
    await sendScenarioPrompt(scenario, false, "first");
    const held = await waitFor(
      ctx,
      "durable_committed_held_write",
      () => fixture.observe(),
      (value) =>
        value.held === 1 &&
        value.effects.executions === before.effects.executions + 1 &&
        value.effects.count === before.effects.count + 1,
    );
    const dispatch = await execution(ctx, scenario, false);
    requireThat(dispatch?.state === "dispatching", "held_write_dispatching");
    const supervisor = new Supervisor(ctx);
    const worker = await supervisor.restart("worker");
    const orchestrator = await supervisor.restart("orchestrator");
    const terminal = await execution(ctx, scenario);
    requireThat(
      terminal?.state === "outcome_unknown" &&
        terminal.executionId === dispatch.executionId,
      "durable_unknown_after_restart",
    );
    const result =
      await scenario.reviewClient.integration.getMcpExecutionResult({
        sessionId: scenario.sessionId,
        executionId: terminal.executionId,
      });
    requireThat(
      result.state === "outcome_unknown" && !result.resultUnavailable,
      "unknown_is_not_success_with_missing_result",
    );
    if (result.resultJson) {
      const receipt = JSON.parse(result.resultJson);
      requireThat(
        receipt.outcome === "outcome_unknown" &&
          receipt.delivery === "unavailable" &&
          receipt.output === undefined,
        "unknown_has_no_invented_output",
      );
    }
    await fixture.release();
    await fixture.configure({ holdCalls: false });
    const first = await waitFor(
      ctx,
      "unknown_guest_receipt",
      () => completions(ctx, scenario, "held-write-1"),
      (records) =>
        records.some(
          (record) =>
            record.proof.outcome === "outcome_unknown" &&
            record.proof.delivery === "unavailable",
        ),
    );
    await sendScenarioPrompt(scenario, false, "first", 2);
    const repeated = await waitFor(
      ctx,
      "duplicate_call_guest_receipt",
      () => completions(ctx, scenario, "held-write-1"),
      (records) => records.length > first.length,
    );
    assertDuplicateWriteUnknown(repeated);
    await sendScenarioPrompt(scenario, false, "second");
    const denied = await waitFor(
      ctx,
      "new_id_restriction_guest_receipt",
      () => completions(ctx, scenario, "held-write-2"),
      (records) => records.length > 0,
    );
    assertNewIdWriteDenial(denied);
    const state = await scenario.reviewClient.integration.getSessionMcpState({
      sessionId: scenario.sessionId,
      omitBindings: true,
      pageSize: 16,
    });
    requireThat(
      !state.hasMoreExecutions &&
        state.unknownWriteBlocks.includes(scenario.connection.id) &&
        state.executions.filter((row) => row.toolCallId === "held-write-1")
          .length === 1 &&
        !state.executions.some(
          (row) =>
            row.toolCallId === "held-write-2" &&
            ["dispatching", "succeeded", "outcome_unknown"].includes(row.state),
        ),
      "unknown_write_replay_restricted",
    );
    const after = await fixture.observe();
    requireThat(
      after.effects.executions === before.effects.executions + 1 &&
        after.effects.count === before.effects.count + 1 &&
        after.held === 0,
      "one_effect_after_both_retries",
    );
    return completeAssertions("held-write-restart", [
      {
        assertion: "committed-held-barrier",
        observations: observations("fixture-observer", "dispatching", held),
      },
      {
        assertion: "worker-restart",
        observations: observations("supervisor", "running", worker),
      },
      {
        assertion: "orchestrator-restart",
        observations: observations("supervisor", "running", orchestrator),
      },
      {
        assertion: "outcome-unknown",
        observations: observations("public-rpc", "outcome_unknown", {
          executionId: terminal.executionId,
          state: terminal.state,
        }),
      },
      {
        assertion: "one-effect",
        observations: observations("fixture-observer", "outcome_unknown", {
          before,
          after,
        }),
      },
      {
        assertion: "duplicate-id-restricted",
        observations: observations("guest-proof", "outcome_unknown", repeated),
      },
      {
        assertion: "new-id-restricted",
        observations: [
          ...observations("guest-proof", "denied", denied),
          ...observations("public-rpc", "denied", {
            blocks: state.unknownWriteBlocks,
            executions: state.executions.map((row) => ({
              id: row.executionId,
              call: row.toolCallId,
              state: row.state,
            })),
          }),
        ],
      },
    ]);
  } finally {
    await fixture.release();
    await fixture.configure({ holdCalls: false });
    await closeScenario(principals, scenario);
  }
};

export const outboxGuestAckDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const fixture = new FixtureObserver(ctx);
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: false,
    revision: 1,
  });
  const observed: Record<string, DriverObservation[]> = {};
  for (const targetGone of [false, true]) {
    const before = await fixture.observe();
    const scenario = await startScenario(ctx, principals, database, {
      harness,
      suffix: `${harness}-outbox-${targetGone ? "gone" : "ack"}`,
      protocol: "modern",
      upstreamName: "write_counter",
      mode: "write",
      caller: "automation",
      automationWrite: true,
      arguments: { delta: 1 },
      constraints: [],
      callId: "outbox-write-1",
      completionBarrier: true,
      expectedNativeResult: expectedCounterNative(
        harness,
        before.effects.count + 1,
        false,
      ),
    });
    const identitySha256 = sha256(
      canonicalJson({
        session: scenario.sessionId,
        call: scenario.input.callId,
      }),
    );
    const observation = (
      source: DriverObservation["source"],
      state: DriverObservation["state"],
      value: unknown,
    ): DriverObservation => ({
      source,
      state,
      identitySha256,
      contentSha256: sha256(canonicalJson(value)),
      ...(source === "public-rpc"
        ? { actorSha256: sha256(scenario.reviewActorId) }
        : {}),
    });
    try {
      await sendScenarioPrompt(scenario);
      const row = await execution(ctx, scenario);
      requireThat(row?.state === "succeeded", "outbox_verified_success");
      const result = await verifiedCounterResult(
        scenario,
        row.executionId,
        before.effects.count + 1,
        false,
      );
      const proofs = await pollProof(ctx, scenario, {
        callId: scenario.input.callId,
        tool: scenario.alias,
      });
      requireThat(
        proofs.length === 1 &&
          proofs[0]!.proof.resultReceived === true &&
          proofs[0]!.proof.resultMatchesExpected === true &&
          proofs[0]!.proof.delivery === "available",
        "outbox_actual_native_result",
      );
      const held = await pollGuestObservation(ctx, scenario, {
        callId: scenario.input.callId,
        tool: scenario.alias,
        observation: "completionBarrierHeld",
      });
      const unacked = await database.outbox(
        scenario.sessionId,
        scenario.input.callId,
      );
      requireThat(
        unacked.relayed &&
          unacked.attempts >= 1 &&
          !unacked.acknowledged &&
          (await toolCompletionEvents(scenario)).length === 0,
        "native_completion_held_before_ack",
      );
      if (!targetGone) {
        const supervisor = new Supervisor(ctx);
        const orchestrator = await supervisor.restart("orchestrator");
        const coordinator = await supervisor.restart("coordinator");
        const recovered = await database.outbox(
          scenario.sessionId,
          scenario.input.callId,
        );
        requireThat(
          recovered.relayed &&
            !recovered.acknowledged &&
            (await toolCompletionEvents(scenario)).length === 0,
          "unacked_delivery_survives_restart",
        );
        await releaseGuestBarrier(
          ctx,
          scenario.client,
          scenario.sessionId,
          "completion",
        );
        const released = await pollGuestObservation(ctx, scenario, {
          callId: scenario.input.callId,
          tool: scenario.alias,
          observation: "completionBarrierReleased",
        });
        const ack = await delivered(ctx, database, scenario);
        requireThat(
          ack.completion.length === 1 &&
            ack.completion[0]!.run_id === row.runId &&
            ack.completion[0]!.ok,
          "ack_exact_native_run_call",
        );
        observed["verified-persisted-result"] = [
          observation("public-rpc", "succeeded", result),
        ];
        observed["coordinator-outbox"] = [
          observation("coordinator-read-only", "relayed", {
            unacked,
            recovered,
          }),
        ];
        observed.restart = [
          observation("supervisor", "running", { orchestrator, coordinator }),
        ];
        observed["guest-proof"] = [
          observation("guest-proof", "succeeded", { proofs, held, released }),
        ];
        observed["guest-ack"] = [
          observation("public-rpc", "acknowledged", ack.completion),
          observation("coordinator-read-only", "acknowledged", {
            outbox: ack.outbox,
            target: ack.target,
          }),
        ];
      } else {
        const recovery = await recoverCurrentBinding(ctx, scenario, database);
        const target = await waitFor(
          ctx,
          "old_delivery_target_retired",
          () =>
            database.deliveryTarget(scenario.sessionId, scenario.input.callId),
          (value) => value.targetGone,
        );
        const settled = await waitFor(
          ctx,
          "target_gone_outbox_settled",
          () => database.outbox(scenario.sessionId, scenario.input.callId),
          (value) => value.acknowledged,
        );
        const afterResult = await verifiedCounterResult(
          scenario,
          row.executionId,
          before.effects.count + 1,
          false,
        );
        requireThat(
          afterResult.resultHash === result.resultHash &&
            afterResult.settlement ===
              McpCompletionSettlement.SETTLED_OR_TARGET_GONE &&
            (await toolCompletionEvents(scenario)).length === 0,
          "target_gone_is_not_guest_ack",
        );
        observed["target-gone-not-delivery"] = [
          observation("coordinator-read-only", "target_gone", {
            target,
            settled,
            old: recovery.old.source_key,
            current: recovery.current.source_key,
          }),
          observation("public-rpc", "target_gone", {
            settlement: afterResult.settlement,
            resultHash: afterResult.resultHash,
            completions: 0,
          }),
        ];
      }
      const after = await fixture.observe();
      requireThat(
        after.effects.executions === before.effects.executions + 1 &&
          after.effects.count === before.effects.count + 1 &&
          after.held === 0,
        "outbox_no_provider_replay",
      );
      (observed["no-provider-replay"] ??= []).push(
        observation("fixture-observer", "succeeded", { before, after }),
      );
    } finally {
      await closeScenario(principals, scenario);
    }
  }
  return completeAssertions(
    "outbox-guest-ack",
    Object.entries(observed).map(([assertion, observations]) => ({
      assertion,
      observations,
    })),
  );
};

export const revokeResumeDriver: CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const fixture = new FixtureObserver(ctx);
  await fixture.configure({
    reply: "json",
    response: "normal",
    holdCalls: false,
    revision: 1,
  });
  const before = await fixture.observe();
  const scenario = await startScenario(ctx, principals, database, {
    harness,
    suffix: `${harness}-revoke-resume`,
    protocol: "modern",
    upstreamName: "read_counter",
    mode: "read",
    caller: "owner",
    automationWrite: false,
    arguments: {},
    constraints: [],
    callId: "resume-read-1",
    callPlan: (alias) => ({
      steps: [
        {
          marker: "initial",
          tool: alias,
          arguments: {},
          callId: "resume-read-1",
          resultFormat: "mcp",
          expectedResult: expectedCounterNative(
            harness,
            before.effects.count,
            false,
          ),
        },
        {
          marker: "old",
          tool: alias,
          arguments: {},
          callId: "resume-read-1",
          resultFormat: "mcp",
          expectExposure: "absent",
        },
        {
          marker: "new",
          tool: alias,
          arguments: {},
          callId: "resume-read-2",
          resultFormat: "mcp",
          expectExposure: "absent",
        },
      ],
    }),
  });
  const identitySha256 = sha256(
    canonicalJson({
      session: scenario.sessionId,
      connection: scenario.connection.id,
    }),
  );
  const observation = (
    source: DriverObservation["source"],
    state: DriverObservation["state"],
    value: unknown,
  ): DriverObservation => ({
    source,
    state,
    identitySha256,
    contentSha256: sha256(canonicalJson(value)),
    ...(source === "public-rpc"
      ? { actorSha256: sha256(scenario.reviewActorId) }
      : {}),
  });
  try {
    await sendScenarioPrompt(scenario, false, "initial");
    const initial = await execution(ctx, scenario);
    requireThat(
      initial?.state === "succeeded",
      "revoke_initial_read_succeeded",
    );
    await verifiedCounterResult(
      scenario,
      initial.executionId,
      before.effects.count,
      false,
    );
    await delivered(ctx, database, scenario);
    const prior = await pollProof(ctx, scenario, {
      callId: scenario.input.callId,
      tool: scenario.alias,
      planSha256: scenario.planSha256,
    });
    requireThat(
      prior.length === 1 && prior[0]!.proof.resultMatchesExpected === true,
      "revoke_initial_actual_proof",
    );
    const profile = await revokeProfileGrants(
      principals.admin,
      scenario.profileId,
    );
    const disconnected = await principals.admin.integration.disconnectMcp({
      connectionId: scenario.connection.id,
      expectedRevision: scenario.connection.configRevision,
      expectedGeneration: scenario.connection.authGeneration,
    });
    requireThat(
      disconnected.connection?.id === scenario.connection.id &&
        disconnected.connection.authGeneration >
          scenario.connection.authGeneration,
      "revoke_actual_credential_generation",
    );
    const recovery = await recoverCurrentBinding(ctx, scenario, database);
    requireThat(
      recovery.candidate.bindingTools.length === 0,
      "revoke_current_empty_binding",
    );
    await sendScenarioPrompt(scenario, false, "old");
    const retried = await pollProof(ctx, scenario, {
      callId: scenario.input.callId,
      tool: scenario.alias,
      planSha256: scenario.planSha256,
      afterCount: prior.length,
    });
    const oldDenied = retried.filter(
      (record) => record.proof.expectExposure === "absent",
    );
    requireThat(
      oldDenied.length === 1 &&
        oldDenied[0]!.proof.requestAttempted === true &&
        oldDenied[0]!.proof.protocolDenialCode === "unknown_tool",
      "old_call_actual_protocol_denial",
    );
    await sendScenarioPrompt(scenario, false, "new");
    const newDenied = await pollProof(ctx, scenario, {
      callId: "resume-read-2",
      tool: scenario.alias,
      planSha256: scenario.planSha256,
    });
    requireThat(
      newDenied.length === 1 &&
        newDenied[0]!.proof.expectExposure === "absent" &&
        newDenied[0]!.proof.requestAttempted === true &&
        newDenied[0]!.proof.protocolDenialCode === "unknown_tool",
      "old_alias_new_id_actual_protocol_denial",
    );
    const emptyExposure = `sha256:${sha256(canonicalJson([]))}`;
    requireThat(
      [...oldDenied, ...newDenied].every(
        (record) => record.proof.exposedMcpToolNamesSha256 === emptyExposure,
      ),
      "resumed_actual_empty_mcp_exposure",
    );
    const after = await fixture.observe();
    const state = await scenario.reviewClient.integration.getSessionMcpState({
      sessionId: scenario.sessionId,
      omitBindings: true,
      pageSize: 16,
    });
    requireThat(
      !state.hasMoreExecutions &&
        state.executions.filter(
          (row) => row.toolCallId === scenario.input.callId,
        ).length === 1 &&
        state.executions.some(
          (row) =>
            row.executionId === initial.executionId &&
            row.state === "succeeded",
        ) &&
        !state.executions.some(
          (row) =>
            row.toolCallId === "resume-read-2" &&
            [
              "authorized",
              "dispatching",
              "succeeded",
              "outcome_unknown",
            ].includes(row.state),
        ) &&
        after.reads === before.reads + 1 &&
        after.effects.executions === before.effects.executions &&
        after.effects.count === before.effects.count,
      "revoke_no_old_call_reexecution",
    );
    return completeAssertions("revoke-resume", [
      {
        assertion: "revocation",
        observations: [
          observation("public-rpc", "denied", {
            profileId: profile.id,
            revision: String(profile.mcpGrantRevision),
            authGeneration: disconnected.connection.authGeneration,
          }),
        ],
      },
      {
        assertion: "actual-resume",
        observations: [
          observation("coordinator-read-only", "started", {
            api: "managed_cold_recovery",
            old: recovery.old.source_key,
            current: recovery.current.source_key,
            ordinaryResumeDenied: recovery.ordinaryResumeDenied,
          }),
        ],
      },
      {
        assertion: "current-binding",
        observations: [
          observation("public-rpc", "current", {
            hash: recovery.finalBindingHash,
            tools: recovery.candidate.bindingTools,
          }),
        ],
      },
      {
        assertion: "old-alias-denied",
        observations: [observation("guest-proof", "denied", newDenied)],
      },
      {
        assertion: "old-call-denied",
        observations: [
          observation("guest-proof", "denied", oldDenied),
          observation("fixture-observer", "denied", { before, after }),
        ],
      },
    ]);
  } finally {
    await closeScenario(principals, scenario);
  }
};
