/** Bearer-authenticated CI dispatch edge for PR reviews (ADR 0100). */

import { Hono } from "hono";

import { getSessionFromHeaders } from "../auth/session.ts";
import { makeEnrollmentStore, type EnrollmentStore } from "../db/enrollments.ts";
import {
  dispatchReview,
  type DispatchReviewInput,
  type DispatchReviewResult,
} from "../workflows/dispatch-review.ts";

type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string } } | null>;

export interface ReviewsDispatchDeps {
  getSession?: GetSession;
  enrollments?: Pick<EnrollmentStore, "get">;
  dispatch?: (input: DispatchReviewInput) => Promise<DispatchReviewResult>;
  randomUUID?: () => string;
}

export function makeReviewsDispatchRoute(
  deps: ReviewsDispatchDeps = {},
): Hono {
  const getSession: GetSession = deps.getSession ?? getSessionFromHeaders;
  let enrollmentStore = deps.enrollments;
  const enrollments = (): Pick<EnrollmentStore, "get"> =>
    (enrollmentStore ??= makeEnrollmentStore());
  const dispatch = deps.dispatch ?? ((input) => dispatchReview({
    enrollments: enrollments(),
  }, input));
  const randomUUID = deps.randomUUID ?? (() => crypto.randomUUID());
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

    if (!(await enrollments().get(repo))) {
      // A missing enrollment is a stable resource miss, not a transient conflict.
      return c.json({ error: "repo is not enrolled" }, 404);
    }
    const result = await dispatch({
      repo,
      prNumber,
      trigger: "dispatch",
      idempotencyKey: randomUUID(),
    });
    if (!result.enrolled || !result.workflowId) {
      return c.json({ error: "repo is not enrolled" }, 404);
    }
    return c.json({
      workflow_id: result.workflowId,
      ...(result.reviewId != null ? { review_id: result.reviewId } : {}),
    });
  });

  return app;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export default makeReviewsDispatchRoute();
