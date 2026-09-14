/** Bearer-authenticated CI dispatch edge for PR reviews (ADR 0100).
 *
 * During the ADR 0119 parallel window the edge follows the repo's `engine`
 * flag like the webhook route does: a repo on the automation engine gets a
 * built-in run (`review.dispatch`), every other repo the legacy ingress. */

import { Hono } from "hono";

import { getSessionFromHeaders } from "../auth/session.ts";
import { config } from "../config.ts";
import { makeEnrollmentStore, type EnrollmentStore } from "../db/enrollments.ts";
import { dispatchAutomationReview, type ReviewCoordinate } from "../reviews/automation-review.ts";
import {
  startReviewIngress,
  type ReviewIngressStart,
} from "../workflows/review-ingress.ts";

type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string } } | null>;

export interface ReviewsDispatchDeps {
  getSession?: GetSession;
  enrollments?: Pick<EnrollmentStore, "get">;
  startIngress?: (input: ReviewIngressStart) => Promise<void>;
  /** Admit a built-in run for the coordinate; returns the run id. */
  dispatchAutomation?: (input: ReviewCoordinate) => Promise<string>;
  /** ADR 0119 phase 4.4 kill switch; defaults to the config value. */
  reviewAutomationDisabled?: boolean;
  randomUUID?: () => string;
}

export function makeReviewsDispatchRoute(
  deps: ReviewsDispatchDeps = {},
): Hono {
  const getSession: GetSession = deps.getSession ?? getSessionFromHeaders;
  let enrollmentStore = deps.enrollments;
  const enrollments = (): Pick<EnrollmentStore, "get"> =>
    (enrollmentStore ??= makeEnrollmentStore());
  const startIngress = deps.startIngress ?? startReviewIngress;
  const dispatchAutomation = deps.dispatchAutomation ?? dispatchAutomationReview;
  const reviewAutomationDisabled = deps.reviewAutomationDisabled ?? config.reviewAutomationDisabled;
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

    const enrollment = await enrollments().get(repo);
    if (!enrollment) {
      // A missing enrollment is a stable resource miss, not a transient conflict.
      return c.json({ error: "repo is not enrolled" }, 404);
    }
    if (enrollment.engine === "automation" && !reviewAutomationDisabled) {
      // The run id is the DBOS workflow id, so the response shape holds.
      const runId = await dispatchAutomation({ repo, prNumber });
      return c.json({ workflow_id: runId });
    }
    // A CI dispatch carries only a coordinate, so ingress resolves the change
    // before any pass starts (ADR 0100 d11). A GitHub blip now delays the review
    // instead of failing it.
    const idempotencyKey = randomUUID();
    await startIngress({
      provider: "github",
      repo,
      prNumber,
      trigger: "dispatch",
      idempotencyKey,
    });
    return c.json({ workflow_id: `review-ingress:${idempotencyKey}` });
  });

  return app;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export default makeReviewsDispatchRoute();
