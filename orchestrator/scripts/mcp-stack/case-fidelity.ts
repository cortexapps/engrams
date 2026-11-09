import {
  requireThat,
  sha256,
  type Context,
  type FixtureKind,
} from "./context.ts";
import { canonicalJson, expectedCounterNative } from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { z } from "zod";
import { bindingTools, waitFor } from "./bootstrap.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import {
  caseBodyFailure,
  completeCaseCleanup,
  closeScenario,
  delivered,
  execution,
  pollProof,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
  type Principals,
  type StartedScenario,
} from "./scenario-runtime.ts";

/** A tool ACK can precede the end of its native turn. */
async function waitForNativeTurn(
  ctx: Pick<Context, "deadlines">,
  scenario: StartedScenario,
  nativeCompletions: Awaited<ReturnType<typeof delivered>>["completion"],
) {
  requireThat(
    nativeCompletions.length > 0,
    "native_turn_requires_tool_completion",
  );
  return waitFor(
    ctx,
    "native_turn_completed",
    async () => {
      const page = await scenario.reviewClient.sessions.listSessionEvents({
        sessionId: scenario.sessionId,
        kinds: ["run_completed"],
        limit: 129n,
      });
      requireThat(page.events.length < 129, "native_turn_event_bound");
      return page.events.some((event) => {
        const idx = event.idx;
        requireThat(
          event.kind === "run_completed" && idx !== undefined,
          "native_turn_event_identity",
        );
        const payload = z
          .object({ run_id: z.string(), ok: z.boolean() })
          .parse(JSON.parse(event.payloadJson));
        return (
          payload.ok &&
          nativeCompletions.every(
            (native) =>
              payload.run_id === native.run_id && idx > BigInt(native.idx),
          )
        );
      });
    },
    (finished) => finished,
  );
}

/** Actual journal, fixture and guest observations; none is a supplied success receipt. */
export async function catalogFidelity(
  ctx: Context,
  principals: Principals,
  database: DatabaseObserver,
  harness: Harness,
) {
  const observations = [];
  for (const protocol of [
    "modern",
    "legacy",
  ] as const satisfies readonly FixtureKind[]) {
    const fixture = new FixtureObserver(ctx, protocol);
    for (const reply of ["json", "sse"] as const) {
      await fixture.configure({
        reply,
        response: "normal",
        holdCalls: false,
        revision: 1,
      });
      const initial = await fixture.observe();
      const scenario = await startScenario(ctx, principals, database, {
        harness,
        suffix: `${harness}-${protocol}-${reply}`,
        protocol,
        upstreamName: "read_counter",
        mode: "read",
        caller: "owner",
        automationWrite: false,
        arguments: {},
        constraints: [],
        callId: "fixture-read-ok",
        callPlan: (alias) => ({
          steps: [false, true].map((isError) => ({
            marker: isError ? "error" : "ok",
            tool: alias,
            arguments: {},
            callId: isError ? "fixture-read-error" : "fixture-read-ok",
            resultFormat: "mcp",
            expectedResult: expectedCounterNative(
              harness,
              initial.effects.count,
              isError,
            ),
          })),
        }),
      });
      try {
        requireThat(scenario.planSha256, "fidelity_call_plan_identity");
        for (const isError of [false, true]) {
          const marker = isError ? "error" : "ok";
          // The normal call is fully verified and acknowledged before this switch.
          if (isError)
            await fixture.configure({
              reply,
              response: "error",
              holdCalls: false,
              revision: 1,
            });
          const before = await fixture.observe();
          const expectedNative = expectedCounterNative(
            harness,
            before.effects.count,
            isError,
          );
          const callScenario: StartedScenario = {
            ...scenario,
            input: { ...scenario.input, callId: `fixture-read-${marker}` },
          };
          await sendScenarioPrompt(callScenario, false, marker);
          const call = await execution(ctx, callScenario);
          requireThat(call, "fidelity_execution");
          const result = await verifiedCounterResult(
            callScenario,
            call.executionId,
            before.effects.count,
            isError,
          );
          const selected = await pollProof(ctx, callScenario, {
            callId: callScenario.input.callId,
            tool: scenario.alias,
            planSha256: scenario.planSha256,
          });
          const completion = await delivered(ctx, database, callScenario);
          const reviewed = scenario.catalog.tools.find(
            (tool) => tool.upstreamName === "read_counter",
          );
          requireThat(
            reviewed && selected.length > 0,
            "fidelity_selected_tool",
          );
          const bound = await bindingTools(scenario.client, scenario.sessionId);
          requireThat(
            bound.length === 1 && bound[0]?.modelToolName === scenario.alias,
            "reviewed_only_binding_set",
          );
          const expectedHash = `sha256:${sha256(canonicalJson(expectedNative))}`;
          const schemaHash = `sha256:${sha256(canonicalJson(JSON.parse(reviewed.inputSchemaJson)))}`;
          requireThat(
            selected.every(
              ({ proof }) =>
                proof.selectedToolName === scenario.alias &&
                proof.exposedMcpToolNamesSha256 ===
                  `sha256:${sha256(canonicalJson([scenario.alias]))}` &&
                proof.selectedSchemaSha256 === schemaHash &&
                proof.delivery === "available" &&
                proof.resultMatchesExpected === true &&
                proof.resultSha256 === expectedHash &&
                proof.expectedResultSha256 === expectedHash,
            ),
            "exact_native_delivery_fidelity",
          );
          requireThat(
            selected.every((record) =>
              completion.proofHashes.includes(record.sha256),
            ),
            "fidelity_selected_proof_delivered",
          );
          const after = await fixture.observe();
          requireThat(
            after.effects.executions === before.effects.executions &&
              after.effects.count === before.effects.count &&
              after.reads === before.reads + 1,
            "one_read_no_effect",
          );
          await waitForNativeTurn(ctx, callScenario, completion.completion);
          observations.push({
            protocol: ctx.protocolFixtures[protocol].protocol,
            reply,
            isError,
            schemaHash,
            expectedHash,
            resultHash: result.resultHash,
            proofHashes: selected.map((record) => record.sha256),
            acknowledged: completion.outbox.acknowledged,
            fixtureBeforeHash: sha256(canonicalJson(before)),
            fixtureAfterHash: sha256(canonicalJson(after)),
          });
        }
      } catch (error) {
        throw caseBodyFailure(error);
      } finally {
        await completeCaseCleanup(async () => {
          await closeScenario(principals, scenario);
        });
      }
    }
  }
  requireThat(observations.length === 8, "all_fidelity_variants");
  return observations;
}

/** All five assertions derive from the eight completed real transport/protocol variants above. */
export const catalogFidelityDriver: import("./driver.ts").CaseDriver = async ({
  ctx,
  principals,
  database,
  harness,
}) => {
  const { completeAssertions } = await import("./driver.ts");
  const rows = await catalogFidelity(ctx, principals, database, harness);
  const publicViews = rows.map((row) => ({
    source: "public-rpc" as const,
    identitySha256: sha256(
      canonicalJson({
        protocol: row.protocol,
        reply: row.reply,
        isError: row.isError,
      }),
    ),
    state: "current" as const,
    contentSha256: sha256(
      canonicalJson({ schemaHash: row.schemaHash, resultHash: row.resultHash }),
    ),
  }));
  const guestViews = rows.flatMap((row) =>
    row.proofHashes.map((hash) => ({
      source: "guest-proof" as const,
      identitySha256: sha256(
        canonicalJson({
          protocol: row.protocol,
          reply: row.reply,
          isError: row.isError,
        }),
      ),
      state: row.isError ? ("failed" as const) : ("succeeded" as const),
      contentSha256: hash,
    })),
  );
  return completeAssertions("catalog-fidelity", [
    { assertion: "reviewed-tool-only-exposure", observations: guestViews },
    {
      assertion: "actual-alias-schema",
      observations: [...publicViews, ...guestViews],
    },
    { assertion: "http-json-sse", observations: publicViews },
    { assertion: "modern-legacy", observations: publicViews },
    { assertion: "exact-text-structured-is-error", observations: guestViews },
  ]);
};
