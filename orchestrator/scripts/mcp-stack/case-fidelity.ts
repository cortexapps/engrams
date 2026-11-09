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
  readGuestProofRecords,
} from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { bindingTools } from "./bootstrap.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import {
  caseBodyFailure,
  completeCaseCleanup,
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
              readGuestProofRecords(
                ctx,
                scenario.client,
                scenario.sessionId,
                path,
              ),
            ),
          );
          const selected = proofs
            .flat()
            .filter((value) => value.proof.resultReceived);
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
