import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
import { makeReviewsDispatchRoute } from "../routes/reviews-dispatch.ts";
import type { ReviewIngressStart } from "../workflows/review-ingress.ts";

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
  const ingresses: ReviewIngressStart[] = [];
  return {
    ingresses,
    app: makeReviewsDispatchRoute({
      getSession: async () => options.authenticated === false
        ? null
        : { user: { id: "api-user" } },
      enrollments: { get: async () => options.enrolled === false ? null : enrollment },
      randomUUID: () => "dispatch-key-1",
      startIngress: async (input) => {
        ingresses.push(input);
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
    expect(fixture.ingresses).toEqual([]);
  });

  test("starts ingress for an enrolled repo and returns its workflow id", async () => {
    const fixture = app();
    const res = await request(fixture.app);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      workflow_id: "review-ingress:dispatch-key-1",
    });
    expect(fixture.ingresses).toEqual([{
      provider: "github",
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "dispatch",
      idempotencyKey: "dispatch-key-1",
    }]);
  });

  test("returns 404 without dispatch for an un-enrolled repo", async () => {
    const fixture = app({ enrolled: false });
    expect((await request(fixture.app)).status).toBe(404);
    expect(fixture.ingresses).toEqual([]);
  });
});
