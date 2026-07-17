/** GitHub App webhook edge: verify, classify, and durably dispatch (ADR 0100). */

import { Hono } from "hono";

import { makeEnrollmentStore, type EnrollmentStore } from "../db/enrollments.ts";
import {
  classifyGithubEvent,
  parseEngramsCommand,
} from "../integrations/github-webhook.ts";
import {
  getGithubWebhookSecret,
  verifyGithubSignature,
} from "../integrations/github.ts";
import { log as rootLog } from "../log.ts";
import {
  dispatchReview,
  type DispatchReviewInput,
  type DispatchReviewResult,
} from "../workflows/dispatch-review.ts";

const log = rootLog.child({ component: "github-webhook" });

export interface GithubEventsDeps {
  webhookSecret?: () => Promise<string>;
  enrollments?: Pick<EnrollmentStore, "get">;
  dispatch?: (input: DispatchReviewInput) => Promise<DispatchReviewResult>;
}

export function makeGithubEventsRoute(deps: GithubEventsDeps = {}): Hono {
  const webhookSecret = deps.webhookSecret ?? getGithubWebhookSecret;
  let enrollmentStore = deps.enrollments;
  const enrollments = (): Pick<EnrollmentStore, "get"> =>
    (enrollmentStore ??= makeEnrollmentStore());
  const dispatch = deps.dispatch ?? ((input) => dispatchReview({
    enrollments: enrollments(),
  }, input));
  const app = new Hono();

  app.post("/api/v1/integrations/github/events", async (c) => {
    const rawBody = await c.req.text();
    const valid = verifyGithubSignature(
      await webhookSecret(),
      rawBody,
      c.req.header("x-hub-signature-256"),
    );
    if (!valid) return c.json({ error: "invalid signature" }, 401);

    const event = classifyGithubEvent(
      c.req.header("x-github-event") ?? "",
      rawBody,
    );
    if (event.kind === "ping") return c.body(null, 200);
    if (event.kind === "ignore") return c.body(null, 200);

    const enrollment = await enrollments().get(event.repo);
    if (!enrollment) return c.body(null, 200);
    const idempotencyKey = c.req.header("x-github-delivery") ?? "";

    if (event.kind === "pull_request") {
      if (event.action === "closed") {
        log.info(
          { repo: event.repo, prNumber: event.prNumber },
          "github PR closed; review halt is deferred",
        );
        return c.body(null, 200);
      }
      if (
        (event.action === "opened" || event.action === "ready_for_review") &&
        (enrollment.triggerMode !== "auto" || event.draft)
      ) {
        return c.body(null, 200);
      }
      await dispatch({
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: event.action === "synchronize" ? "synchronize" : "opened",
        idempotencyKey,
        headSha: event.headSha,
      });
      return c.body(null, 200);
    }

    const command = parseEngramsCommand(event.body);
    if (command?.kind === "review") {
      await dispatch({
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: "command",
        idempotencyKey,
        ...(command.focus != null ? { focus: command.focus } : {}),
      });
    } else if (command?.kind === "stop") {
      await dispatch({
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: "command",
        idempotencyKey,
        stop: true,
      });
    } else if (command?.kind === "fix") {
      log.info(
        { repo: event.repo, prNumber: event.prNumber, commentId: event.commentId },
        "github @engrams fix queued for a later PR",
      );
    }
    return c.body(null, 200);
  });

  return app;
}

export default makeGithubEventsRoute();
