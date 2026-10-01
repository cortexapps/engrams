import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
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
  const automationDispatches: Array<{ repo: string; prNumber: number }> = [];
  return {
    automationDispatches,
    app: makeReviewsDispatchRoute({
      getSession: async () => options.authenticated === false
        ? null
        : { user: { id: "api-user" } },
      enrollments: { get: async () => options.enrolled === false ? null : enrollment },
      dispatchAutomation: async (input) => {
        automationDispatches.push(input);
        return "autorun:b1:dispatch:uuid-1";
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
    expect(fixture.automationDispatches).toEqual([]);
  });

  test("an enrolled repo gets a built-in run and its run id back", async () => {
    const fixture = app();
    const res = await request(fixture.app);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ workflow_id: "autorun:b1:dispatch:uuid-1" });
    expect(fixture.automationDispatches).toEqual([{ repo: enrollment.repo, prNumber: 100 }]);
  });

  test("returns 404 without dispatch for an un-enrolled repo", async () => {
    const fixture = app({ enrolled: false });
    expect((await request(fixture.app)).status).toBe(404);
    expect(fixture.automationDispatches).toEqual([]);
  });
});
