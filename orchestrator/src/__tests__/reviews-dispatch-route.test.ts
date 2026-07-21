import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
import type { DispatchReviewInput } from "../workflows/dispatch-review.ts";
import { makeReviewsDispatchRoute } from "../routes/reviews-dispatch.ts";

const PATH = "/api/v1/reviews/dispatch";
const enrollment: EnrollmentRow = {
  repo: "openai/engrams",
  triggerMode: "manual",
  autofix: "off",
  profileId: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

function app(options: { authenticated?: boolean; enrolled?: boolean } = {}) {
  const dispatches: DispatchReviewInput[] = [];
  return {
    dispatches,
    app: makeReviewsDispatchRoute({
      getSession: async () => options.authenticated === false
        ? null
        : { user: { id: "api-user" } },
      enrollments: { get: async () => options.enrolled === false ? null : enrollment },
      randomUUID: () => "dispatch-key-1",
      dispatch: async (input) => {
        dispatches.push(input);
        return {
          enrolled: true,
          workflowId: "review:workflow-1",
          reviewId: "review-row-1",
        };
      },
    }),
  };
}

function request(route: ReturnType<typeof app>["app"]) {
  return route.request(PATH, {
    method: "POST",
    headers: {
      "authorization": "Bearer engk_test",
      "content-type": "application/json",
    },
    body: JSON.stringify({ repo: enrollment.repo, pr_number: 100 }),
  });
}

describe("POST /api/v1/reviews/dispatch", () => {
  test("requires authentication", async () => {
    const fixture = app({ authenticated: false });
    expect((await request(fixture.app)).status).toBe(401);
    expect(fixture.dispatches).toEqual([]);
  });

  test("dispatches an enrolled repo and returns workflow/review ids", async () => {
    const fixture = app();
    const res = await request(fixture.app);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      workflow_id: "review:workflow-1",
      review_id: "review-row-1",
    });
    expect(fixture.dispatches).toEqual([{
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "dispatch",
      idempotencyKey: "dispatch-key-1",
    }]);
  });

  test("returns 404 without dispatch for an un-enrolled repo", async () => {
    const fixture = app({ enrolled: false });
    expect((await request(fixture.app)).status).toBe(404);
    expect(fixture.dispatches).toEqual([]);
  });
});
