import { requireThat, sha256 } from "./context.ts";
import { waitFor } from "./bootstrap.ts";
import { canonicalJson, proofPaths, readGuestProofRecords } from "./guest.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverObservation,
} from "./driver.ts";
import { FixtureObserver } from "./observer.ts";
import { Supervisor } from "./supervisor.ts";
import {
  closeScenario,
  execution,
  sendScenarioPrompt,
  startScenario,
  type StartedScenario,
} from "./scenario-runtime.ts";
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
    requireThat(
      repeated.at(-1)?.proof.outcome === "outcome_unknown" &&
        repeated.at(-1)?.proof.delivery === "unavailable",
      "duplicate_id_stays_unknown",
    );
    await sendScenarioPrompt(scenario, false, "second");
    const denied = await waitFor(
      ctx,
      "new_id_restriction_guest_receipt",
      () => completions(ctx, scenario, "held-write-2"),
      (records) => records.length > 0,
    );
    requireThat(
      denied.every(
        (record) =>
          record.proof.delivery === "unavailable" &&
          record.proof.outcome !== "succeeded",
      ),
      "new_id_not_success",
    );
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
