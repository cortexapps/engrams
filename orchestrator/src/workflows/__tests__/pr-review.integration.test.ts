/**
 * Integration test: drive the REAL PrReviewWorkflow through an embedded DBOS
 * engine, not injected fakes. This is the coverage the unit test cannot give —
 * it proves the extracted module functions (beginReview/setupPhase/advanceReview)
 * actually call DBOS.runStep / recv / send through the engine's async-local
 * workflow context, that the mailbox delivers trigger + phase signals in order,
 * and that the workflow checkpoints and reaches a terminal status.
 *
 * Live-PG gated exactly like the other orchestrator DB tests: it runs in the CI
 * `orchestrator` lane (which provides Postgres + ORCHESTRATOR_DATABASE_URL and
 * lets DBOS.launch create the `dbos` schema) and self-skips locally without a
 * reachable DB.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { DBOS } from "@dbos-inc/dbos-sdk";

import { checkDb } from "../../db/client.ts";
import type { ReviewControlPlane } from "../review-control-plane.ts";
import { prReviewWorkflow, setReviewControlPlane } from "../pr-review.ts";
import { REVIEW_TOPIC, type ReviewInbox } from "../review-inbox.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

/** A control plane that records the method call order and returns canned data;
 *  `candidateCount` drives whether the finder hands off to a verifier. */
function recordingControlPlane(
  calls: string[],
  candidateCount: number,
): ReviewControlPlane {
  const rec = <T>(name: string, value: T): T => {
    calls.push(name);
    return value;
  };
  return {
    resolvePrHeads: async () => {
      throw new Error("pass workflow must not resolve pull requests");
    },
    resolveReviewTarget: async () => {
      throw new Error("pass workflow must not resolve targets");
    },
    createReviewPass: async () => {
      throw new Error("pass workflow must not create review rows");
    },
    updateReviewPassContext: async () => {
      throw new Error("pass workflow must not update ingress context");
    },
    startReviewPass: async () => {
      throw new Error("pass workflow must not dispatch itself");
    },
    abandonIngress: async () => {
      throw new Error("pass workflow must not abandon ingress");
    },
    acknowledgeReviewPass: async () => {
      throw new Error("pass workflow must not acknowledge ingress passes");
    },
    signalSupersededPass: async () => {
      throw new Error("pass workflow must not signal predecessors");
    },
    createFinderSession: async () => rec("createFinderSession", { sessionId: "finder-1" }),
    bootstrapFinderSession: async () => { rec("bootstrapFinderSession", undefined); },
    sendFinderPrompt: async () => { rec("sendFinderPrompt", undefined); },
    concludeFinderPhase: async () => rec("concludeFinderPhase", { candidateCount }),
    createVerifierSession: async () => rec("createVerifierSession", { sessionId: "verifier-1" }),
    bootstrapVerifierSession: async () => { rec("bootstrapVerifierSession", undefined); },
    sendVerifierPrompt: async () => { rec("sendVerifierPrompt", undefined); },
    postReviewResults: async () => { rec("postReviewResults", undefined); },
    failReview: async () => { rec("failReview", undefined); },
    haltReview: async () => { rec("haltReview", undefined); },
    cleanupSupersededReview: async () => {
      rec("cleanupSupersededReview", undefined);
    },
  };
}

const trigger: ReviewInbox = {
  kind: "trigger",
  reviewId: "rev-1",
  taskId: "task-1",
  repo: "acme/repo",
  prNumber: 7,
  trigger: "opened",
  headSha: "h",
  baseSha: "b",
};

async function runToTerminal(
  messages: ReviewInbox[],
): Promise<{ status: string | undefined }> {
  const workflowId = `it-review-${crypto.randomUUID()}`;
  const handle = await DBOS.startWorkflow(prReviewWorkflow, { workflowID: workflowId })();
  for (const [i, message] of messages.entries()) {
    await DBOS.send<ReviewInbox>(workflowId, message, REVIEW_TOPIC, `${workflowId}:${i}`);
  }
  await handle.getResult();
  const status = await DBOS.getWorkflowStatus(workflowId);
  return { status: status?.status };
}

describe.skipIf(!dbReachable)("PrReviewWorkflow (real DBOS engine)", () => {
  beforeAll(async () => {
    DBOS.setConfig({
      name: "engrams-orchestrator",
      systemDatabaseUrl: DB_URL!,
      systemDatabaseSchemaName: "dbos",
      runAdminServer: false,
    });
    await DBOS.launch();
  });

  afterAll(async () => {
    await DBOS.shutdown();
    setReviewControlPlane(undefined);
  });

  test("finder → verifier → post drives to completion", async () => {
    const calls: string[] = [];
    setReviewControlPlane(recordingControlPlane(calls, 1));
    const { status } = await runToTerminal([
      trigger,
      { kind: "phase_done", role: "finder" },
      { kind: "phase_done", role: "verifier" },
    ]);
    expect(status).toBe("SUCCESS");
    expect(calls).toEqual([
      "createFinderSession", "bootstrapFinderSession", "sendFinderPrompt",
      "concludeFinderPhase",
      "createVerifierSession", "bootstrapVerifierSession", "sendVerifierPrompt",
      "postReviewResults",
    ]);
  }, 60_000);

  test("zero candidates posts directly without a verifier", async () => {
    const calls: string[] = [];
    setReviewControlPlane(recordingControlPlane(calls, 0));
    const { status } = await runToTerminal([
      trigger,
      { kind: "phase_done", role: "finder" },
    ]);
    expect(status).toBe("SUCCESS");
    expect(calls).not.toContain("createVerifierSession");
    expect(calls.at(-1)).toBe("postReviewResults");
  }, 60_000);

  test("a dead finder session fails the review with no retry", async () => {
    const calls: string[] = [];
    setReviewControlPlane(recordingControlPlane(calls, 1));
    const { status } = await runToTerminal([
      trigger,
      { kind: "session_ended", role: "finder", sessionId: "finder-1", outcome: "failed" },
    ]);
    // The workflow returns normally (SUCCESS) after failing the review; it does
    // not spin up a replacement finder or a verifier.
    expect(status).toBe("SUCCESS");
    expect(calls).toContain("failReview");
    expect(calls.filter((c) => c === "createFinderSession")).toHaveLength(1);
    expect(calls).not.toContain("createVerifierSession");
  }, 60_000);
});
