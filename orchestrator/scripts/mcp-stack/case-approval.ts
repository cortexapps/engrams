import { requireThat, sha256, type Context } from "./context.ts";
import { canonicalJson, expectedCounterNative } from "./guest.ts";
import type { Harness } from "./manifest.ts";
import { DatabaseObserver, FixtureObserver } from "./observer.ts";
import {
  caseBodyFailure,
  completeCaseCleanup,
  approveOnce,
  closeScenario,
  delivered,
  execution,
  sendScenarioPrompt,
  startScenario,
  verifiedCounterResult,
  type Principals,
} from "./scenario-runtime.ts";

export async function interactiveApproval(
  ctx: Context,
  principals: Principals,
  database: DatabaseObserver,
  harness: Harness,
) {
  const observations = [];
  for (const actor of ["owner", "admin"] as const) {
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
      suffix: `${harness}-approval-${actor}`,
      protocol: "modern",
      upstreamName: "write_counter",
      mode: "write",
      caller: "owner",
      automationWrite: false,
      arguments: { delta: 1 },
      constraints: [
        {
          type: "object",
          properties: { delta: { const: 1 } },
          required: ["delta"],
          additionalProperties: false,
        },
      ],
      callId: "fixture-write",
      expectedNativeResult: expectedCounterNative(
        harness,
        before.effects.count + 1,
        false,
      ),
    });
    try {
      await sendScenarioPrompt(scenario);
      const approval = await approveOnce(ctx, principals, scenario, actor);
      const call = await execution(ctx, scenario);
      requireThat(
        call?.executionId === approval.executionId,
        "approved_execution_identity",
      );
      const result = await verifiedCounterResult(
        scenario,
        call.executionId,
        before.effects.count + 1,
        false,
      );
      const completion = await delivered(ctx, database, scenario);
      const after = await fixture.observe();
      requireThat(
        after.effects.executions === before.effects.executions + 1 &&
          after.effects.count === before.effects.count + 1,
        "one_approved_write_effect",
      );
      observations.push({
        protocol: ctx.protocolFixtures.modern.protocol,
        actor,
        approvalContextHash: approval.contextHash,
        rejections: approval.rejections,
        resultHash: result.resultHash,
        proofHashes: completion.proofHashes,
        acknowledged: completion.outbox.acknowledged,
        beforeHash: sha256(canonicalJson(before)),
        afterHash: sha256(canonicalJson(after)),
      });
    } catch (error) {
      throw caseBodyFailure(error);
    } finally {
      await completeCaseCleanup(async () => {
        await closeScenario(principals, scenario);
      });
    }
  }
  requireThat(observations.length === 2, "owner_and_admin_approvals");
  return observations;
}

export const interactiveApprovalDriver: import("./driver.ts").CaseDriver =
  async ({ ctx, principals, database, harness }) => {
    const { completeAssertions } = await import("./driver.ts");
    const rows = await interactiveApproval(ctx, principals, database, harness);
    const views = rows.map((row) => ({
      source: "public-rpc" as const,
      identitySha256: row.approvalContextHash,
      state: "authorized" as const,
      contentSha256: sha256(
        canonicalJson({ actor: row.actor, resultHash: row.resultHash }),
      ),
    }));
    const effects = rows.map((row) => ({
      source: "fixture-observer" as const,
      identitySha256: row.approvalContextHash,
      state: "succeeded" as const,
      contentSha256: sha256(
        canonicalJson({ before: row.beforeHash, after: row.afterHash }),
      ),
      count: 1,
    }));
    const denied = (field: "otherDenied" | "changedDenied" | "replayDenied") =>
      rows.map((row) => ({
        source: "public-rpc" as const,
        identitySha256: row.approvalContextHash,
        state: "denied" as const,
        contentSha256: sha256(
          canonicalJson({ field, code: row.rejections[field] }),
        ),
      }));
    return completeAssertions("interactive-approval", [
      { assertion: "owner-one-use", observations: [views[0]!] },
      { assertion: "admin-one-use", observations: [views[1]!] },
      { assertion: "other-member-denied", observations: denied("otherDenied") },
      {
        assertion: "changed-context-denied",
        observations: denied("changedDenied"),
      },
      {
        assertion: "replayed-decision-denied",
        observations: denied("replayDenied"),
      },
      { assertion: "one-effect", observations: effects },
    ]);
  };
