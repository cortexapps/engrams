import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../../db/enrollments.ts";
import {
  dispatchReview,
  type DispatchDbosOps,
} from "../dispatch-review.ts";
import { reviewHash } from "../review-workflow-id.ts";

const enrollment: EnrollmentRow = {
  repo: "openai/engrams",
  triggerMode: "manual",
  autofix: "off",
  profileId: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

function recordingDbos() {
  const starts: string[] = [];
  const sends: Parameters<DispatchDbosOps["send"]>[] = [];
  const ops: DispatchDbosOps = {
    getWorkflowStatus: async () => null,
    startWorkflow: async (workflowId) => void starts.push(workflowId),
    send: async (...args) => void sends.push(args),
  };
  return { ops, starts, sends };
}

describe("dispatchReview", () => {
  test("drops an un-enrolled repo without touching DBOS", async () => {
    const dbos = recordingDbos();
    const result = await dispatchReview({
      enrollments: { get: async () => null },
      dbos: dbos.ops,
    }, {
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "dispatch",
      idempotencyKey: "dispatch-1",
    });
    expect(result).toEqual({ enrolled: false });
    expect(dbos.starts).toEqual([]);
    expect(dbos.sends).toEqual([]);
  });

  test("starts then sends the trigger to the deterministic workflow", async () => {
    const dbos = recordingDbos();
    const result = await dispatchReview({
      enrollments: { get: async () => enrollment },
      dbos: dbos.ops,
    }, {
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "delivery-1",
      focus: "auth",
    });
    const workflowId = `review:${reviewHash(enrollment.repo, 100)}`;
    expect(result).toEqual({ enrolled: true, workflowId });
    expect(dbos.starts).toEqual([workflowId]);
    expect(dbos.sends).toEqual([[
      workflowId,
      {
        kind: "trigger",
        repo: enrollment.repo,
        prNumber: 100,
        trigger: "command",
        focus: "auth",
      },
      "review",
      "delivery-1",
    ]]);
  });
});
