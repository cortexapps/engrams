/** Bearer-authenticated CI dispatch edge for PR reviews (ADR 0100): an
 * enrolled repo gets a built-in run under the `review.dispatch` event. */

import { Hono } from "hono";

import { getSessionFromHeaders } from "../auth/session.ts";
import { makeAutomationStore } from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { dispatchAutomationReview, type ReviewCoordinate } from "../reviews/automation-review.ts";
import { makeEnrolledRepos, type EnrolledRepos } from "../reviews/enrolled-repos.ts";

type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string } } | null>;

export interface ReviewsDispatchDeps {
  getSession?: GetSession;
  /** The PR-review built-in's `repos` input: which repos get reviews. */
  enrolledRepos?: Pick<EnrolledRepos, "get">;
  /** Admit a built-in run for the coordinate; returns the run id. */
  dispatchAutomation?: (input: ReviewCoordinate) => Promise<string>;
}

export function makeReviewsDispatchRoute(
  deps: ReviewsDispatchDeps = {},
): Hono {
  const getSession: GetSession = deps.getSession ?? getSessionFromHeaders;
  let enrolled = deps.enrolledRepos;
  const enrolledRepos = (): Pick<EnrolledRepos, "get"> =>
    (enrolled ??= makeEnrolledRepos(makeAutomationStore(getDb())));
  const dispatchAutomation = deps.dispatchAutomation ?? dispatchAutomationReview;
  const app = new Hono();

  app.post("/api/v1/reviews/dispatch", async (c) => {
    const session = await getSession(c.req.raw.headers);
    if (!session) return c.json({ error: "unauthenticated" }, 401);

    const body: unknown = await c.req.json().catch(() => null);
    if (!isRecord(body)) return c.json({ error: "invalid request body" }, 400);
    const repo = typeof body["repo"] === "string" ? body["repo"].trim() : "";
    const prNumber = body["pr_number"];
    if (
      !/^[^/\s]+\/[^/\s]+$/.test(repo) ||
      typeof prNumber !== "number" ||
      !Number.isSafeInteger(prNumber) ||
      prNumber <= 0
    ) {
      return c.json({ error: "repo and positive pr_number are required" }, 400);
    }

    if (!(await enrolledRepos().get(repo))) {
      // A repo the built-in does not list is a stable resource miss, not a
      // transient conflict.
      return c.json({ error: "repo is not enrolled" }, 404);
    }
    // The run id is the DBOS workflow id, so the response shape holds.
    const runId = await dispatchAutomation({ repo, prNumber });
    return c.json({ workflow_id: runId });
  });

  return app;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export default makeReviewsDispatchRoute();
