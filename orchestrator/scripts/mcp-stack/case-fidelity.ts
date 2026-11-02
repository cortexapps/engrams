import {
  requireThat,
  sha256,
  type Context,
  type FixtureKind,
} from "./context.ts";
import {
  canonicalJson,
  expectedCounterNative,
  proofPaths,
  readGuestProof,
} from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import {
  closeScenario,
  delivered,
  execution,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
  type Principals,
} from "./scenario-runtime.ts";

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
    for (const reply of ["json", "sse"] as const)
      for (const isError of [false, true]) {
        await fixture.configure({
          reply,
          response: isError ? "error" : "normal",
          holdCalls: false,
          revision: 1,
        });
        const before = await fixture.observe();
        const expectedNative = expectedCounterNative(
          harness,
          before.effects.count,
          isError,
        );
        const scenario = await startScenario(ctx, principals, database, {
          harness,
          suffix: `${harness}-${protocol}-${reply}-${isError ? "error" : "ok"}`,
          protocol,
          upstreamName: "read_counter",
          mode: "read",
          caller: "owner",
          automationWrite: false,
          arguments: {},
          constraints: [],
          callId: "fixture-read",
          expectedNativeResult: expectedNative,
        });
        try {
          await sendScenarioPrompt(scenario);
          const call = await execution(ctx, scenario);
          requireThat(call, "fidelity_execution");
          const result = await verifiedCounterResult(
            scenario,
            call.executionId,
            before.effects.count,
            isError,
          );
          const completion = await delivered(ctx, database, scenario);
          const paths = await proofPaths(
            ctx,
            scenario.client,
            scenario.sessionId,
            scenario.input.suffix,
          );
          const proofs = await Promise.all(
            paths.map((path) =>
              readGuestProof(ctx, scenario.client, scenario.sessionId, path),
            ),
          );
          const selected = proofs
            .filter((value) => value !== null)
            .filter((value) => value.proof.resultReceived);
          const reviewed = scenario.catalog.tools.find(
            (tool) => tool.upstreamName === "read_counter",
          );
          requireThat(
            reviewed && selected.length > 0,
            "fidelity_selected_tool",
          );
          const expectedHash = `sha256:${sha256(canonicalJson(expectedNative))}`;
          const schemaHash = `sha256:${sha256(canonicalJson(JSON.parse(reviewed.inputSchemaJson)))}`;
          requireThat(
            selected.every(
              ({ proof }) =>
                proof.selectedToolName === scenario.alias &&
                proof.selectedSchemaSha256 === schemaHash &&
                proof.delivery === "available" &&
                proof.resultMatchesExpected === true &&
                proof.resultSha256 === expectedHash &&
                proof.expectedResultSha256 === expectedHash,
            ),
            "exact_native_delivery_fidelity",
          );
          const after = await fixture.observe();
          requireThat(
            after.effects.executions === before.effects.executions &&
              after.effects.count === before.effects.count &&
              after.reads === before.reads + 1,
            "one_read_no_effect",
          );
          observations.push({
            protocol: ctx.protocolFixtures[protocol].protocol,
            reply,
            isError,
            schemaHash,
            expectedHash,
            resultHash: result.resultHash,
            proofHashes: completion.proofHashes,
            acknowledged: completion.outbox.acknowledged,
            fixtureBeforeHash: sha256(canonicalJson(before)),
            fixtureAfterHash: sha256(canonicalJson(after)),
          });
        } finally {
          await closeScenario(principals, scenario);
        }
      }
  }
  requireThat(observations.length === 8, "all_fidelity_variants");
  // The caller must still verify actual complete tool exposure before assigning this case PASS.
  return observations;
}
