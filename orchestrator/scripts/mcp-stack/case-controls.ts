import { readOperatorPublication, waitFor } from "./bootstrap.ts";
import { canonicalJson } from "./guest.ts";
import { requireThat, RunnerFailure } from "./context.ts";
import {
  completeAssertions,
  type CaseDriver,
  type DriverAssertion,
} from "./driver.ts";
import {
  approveOnce,
  closeScenario,
  execution,
  pollProof,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
} from "./scenario-runtime.ts";
import { Supervisor } from "./supervisor.ts";
import { deltaOne, deniedCall, policyObservation } from "./case-grants.ts";

export function requireKillVariants(supervisor: {
  configurations: { orchestrator: object };
}) {
  for (const variant of ["normal", "global-disabled", "zone-disabled"])
    if (!(variant in supervisor.configurations.orchestrator))
      throw new RunnerFailure(
        "NEEDS_CONTEXT",
        `supervisor_orchestrator_${variant}_configuration`,
      );
}
export function assertKillPublication(
  before: Awaited<ReturnType<typeof readOperatorPublication>>,
  after: Awaited<ReturnType<typeof readOperatorPublication>>,
) {
  requireThat(
    before.admission_open &&
      before.pending === null &&
      before.published &&
      canonicalJson(before) === canonicalJson(after),
    "kill_preserves_actual_operator_publication",
  );
}
export function cancellationRequest(call: {
  executionId: string;
  journalRevision: number;
  state: string;
}) {
  requireThat(
    call.executionId.length > 0 &&
      Number.isSafeInteger(call.journalRevision) &&
      call.journalRevision > 0 &&
      call.state === "dispatching",
    "current_dispatching_cancellation_revision",
  );
  return {
    executionId: call.executionId,
    expectedJournalRevision: call.journalRevision,
  };
}
export function assertHistoryPreserved(
  before: { state: string; resultUnavailable: boolean; resultJson?: string },
  after: typeof before,
) {
  requireThat(
    before.state === "succeeded" &&
      !before.resultUnavailable &&
      before.resultJson &&
      after.state === before.state &&
      after.resultUnavailable === before.resultUnavailable &&
      after.resultJson === before.resultJson,
    "verified_history_preserved",
  );
}

export const killCancelDriver: CaseDriver = async (input) => {
  const { ctx, principals, database, harness } = input;
  requireKillVariants(ctx.supervisor);
  const supervisor = new Supervisor(ctx);
  const assertions: DriverAssertion[] = [];
  const historyObservations = [];
  const deniedObservations = [];
  for (const control of [
    "global",
    "zone",
    "connection",
    "tool",
    "cancel",
  ] as const) {
    const scenario = await startScenario(ctx, principals, database, {
      harness,
      suffix: `${harness}-control-${control}`,
      protocol: "modern",
      upstreamName: "write_counter",
      mode: "read",
      caller: "owner",
      automationWrite: false,
      arguments: { delta: 1 },
      constraints: [deltaOne],
      callId: "control-history",
      callPlan: (alias) => ({
        steps: [
          {
            marker: "history",
            tool: alias,
            arguments: { delta: 1 },
            callId: "control-history",
            resultFormat: "mcp",
          },
          {
            marker: "held",
            tool: alias,
            arguments: { delta: 1 },
            callId: "control-held",
            resultFormat: "mcp",
          },
          {
            marker: "blocked",
            tool: alias,
            arguments: { delta: 1 },
            callId: "control-blocked",
            resultFormat: "mcp",
          },
        ],
      }),
    });
    let variantChanged = false;
    let publicationBefore:
      | Awaited<ReturnType<typeof readOperatorPublication>>
      | undefined;
    try {
      await scenario.fixture.configure({
        reply: "json",
        response: "normal",
        holdCalls: false,
        revision: 1,
      });
      const before = await scenario.fixture.observe();
      await sendScenarioPrompt(scenario, false, "history");
      await approveOnce(ctx, principals, scenario, "owner");
      const first = await execution(ctx, scenario);
      requireThat(
        first?.state === "succeeded",
        "control_verified_history_exists",
      );
      await verifiedCounterResult(
        scenario,
        first.executionId,
        before.effects.count + 1,
        false,
      );
      await pollProof(ctx, scenario, {
        callId: "control-history",
        tool: scenario.alias,
        planSha256: scenario.planSha256,
      });
      const history = await principals.admin.integration.getMcpExecutionResult({
        sessionId: scenario.sessionId,
        executionId: first.executionId,
      });
      // Commit a real write, then keep its response pending while the control changes.
      await scenario.fixture.configure({ holdCalls: true });
      await sendScenarioPrompt(scenario, false, "held");
      await approveOnce(
        ctx,
        principals,
        { ...scenario, input: { ...scenario.input, callId: "control-held" } },
        "owner",
      );
      const held = await waitFor(
        ctx,
        "control_held_effect",
        () => scenario.fixture.observe(),
        (value) =>
          value.held === 1 &&
          value.effects.executions === before.effects.executions + 2,
      );
      requireThat(
        held.effects.count === before.effects.count + 2,
        "held_write_committed_before_control",
      );
      const inflight = await execution(ctx, scenario, false, "control-held");
      requireThat(
        inflight?.state === "dispatching",
        "actual_inflight_control_target",
      );
      let mutation: unknown;
      if (control === "global" || control === "zone") {
        publicationBefore = await readOperatorPublication(ctx);
        requireThat(
          publicationBefore.published?.identity ===
            ctx.publication.candidateIdentity,
          "kill_normal_publication",
        );
        variantChanged = true;
        const variant =
          control === "global" ? "global-disabled" : "zone-disabled";
        const status = await supervisor.restart("orchestrator", variant);
        const publication = await readOperatorPublication(ctx);
        assertKillPublication(publicationBefore, publication);
        mutation = { configuration: variant, roles: status.roles, publication };
      } else if (control === "connection") {
        const setup = await principals.admin.integration.getMcpSetup({
          connectionId: scenario.connection.id,
        });
        requireThat(
          setup.current?.connection,
          "connection_kill_current_revision",
        );
        const killed = await principals.admin.integration.setConnectionEnabled({
          id: scenario.connection.id,
          enabled: false,
          expectedRevision: setup.current.connection.configRevision,
        });
        requireThat(
          killed.connection?.enabled === false,
          "connection_kill_applied",
        );
        mutation = {
          revision: killed.connection.configRevision,
          enabled: killed.connection.enabled,
        };
      } else if (control === "tool") {
        const setup = await principals.admin.integration.getMcpSetup({
          connectionId: scenario.connection.id,
        });
        requireThat(setup.current?.connection, "tool_kill_current_connection");
        const discovery = await principals.admin.integration.discoverMcp({
          connectionId: scenario.connection.id,
          expectedRevision: setup.current.connection.configRevision,
        });
        requireThat(discovery.catalog, "tool_kill_reviewed_draft");
        const catalogs = await principals.admin.integration.getMcpCatalogs({
          connectionId: scenario.connection.id,
        });
        const published = await principals.admin.integration.publishMcp({
          connectionId: scenario.connection.id,
          draftId: discovery.catalog.id,
          expectedRevision: catalogs.catalogRevision,
          tools: discovery.catalog.tools.map((tool) => ({
            ...tool,
            classification:
              tool.upstreamName === "read_counter" ? "read" : "write",
            enabled: tool.upstreamName !== "write_counter",
          })),
        });
        requireThat(
          published.catalog?.tools.find(
            (tool) => tool.upstreamName === "write_counter",
          )?.enabled === false,
          "tool_kill_applied",
        );
        mutation = {
          revision: published.catalog.revision,
          fingerprint: published.catalog.fingerprint,
        };
      } else {
        const canceled = await principals.owner.integration.cancelMcpCall(
          cancellationRequest(inflight),
        );
        requireThat(
          canceled.receipt?.executionId === inflight.executionId &&
            canceled.receipt.expectedJournalRevision ===
              inflight.journalRevision &&
            canceled.receipt.actorId === principals.actorIds.owner,
          "actual_cancel_receipt",
        );
        mutation = {
          state: canceled.state,
          revision: canceled.receipt.journalRevision,
          cause: canceled.receipt.cause,
        };
      }
      const terminal = await execution(ctx, scenario, true, "control-held");
      requireThat(
        terminal && ["canceled", "outcome_unknown"].includes(terminal.state),
        "control_interrupts_held_call",
      );
      const aborted = await waitFor(
        ctx,
        "control_upstream_abort",
        () => scenario.fixture.observe(),
        (value) => value.held === 0 && value.cancellations > held.cancellations,
      );
      requireThat(
        aborted.effects.executions === held.effects.executions &&
          aborted.effects.count === held.effects.count,
        "control_does_not_undo_committed_effect",
      );
      const afterHistory =
        await principals.admin.integration.getMcpExecutionResult({
          sessionId: scenario.sessionId,
          executionId: first.executionId,
        });
      assertHistoryPreserved(history, afterHistory);
      historyObservations.push(
        policyObservation("public-rpc", first.executionId, "succeeded", {
          state: afterHistory.state,
          result: afterHistory.resultJson,
        }),
      );
      const denied = await deniedCall(
        input,
        scenario,
        "blocked",
        "control-blocked",
      );
      deniedObservations.push(...denied);
      assertions.push({
        assertion: control === "cancel" ? "cancel" : `${control}-kill`,
        observations: [
          policyObservation(
            "public-rpc",
            inflight.executionId,
            terminal.state === "canceled" ? "canceled" : "outcome_unknown",
            mutation,
          ),
          policyObservation(
            "fixture-observer",
            scenario.sessionId,
            "canceled",
            { held, aborted },
            aborted.cancellations - held.cancellations,
          ),
        ],
      });
    } finally {
      try {
        await scenario.fixture.release();
        await scenario.fixture.configure({ holdCalls: false });
      } finally {
        try {
          if (variantChanged) {
            await supervisor.restart("orchestrator", "normal");
            requireThat(publicationBefore, "kill_publication_baseline");
            assertKillPublication(
              publicationBefore,
              await readOperatorPublication(ctx),
            );
          }
        } finally {
          await closeScenario(principals, scenario);
        }
      }
    }
  }
  assertions.push(
    {
      assertion: "journal-history-preserved",
      observations: historyObservations,
    },
    { assertion: "no-new-dispatch", observations: deniedObservations },
  );
  return completeAssertions("kill-cancel", assertions);
};
