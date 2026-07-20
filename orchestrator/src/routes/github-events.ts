/** GitHub App webhook edge: verify, classify, and durably dispatch (ADR 0100). */

import { Hono } from "hono";

import { config } from "../config.ts";
import { makeEnrollmentStore, type EnrollmentStore } from "../db/enrollments.ts";
import {
  classifyGithubEvent,
  parseReviewCommand,
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
const MAX_WEBHOOK_BODY_BYTES = 2 * 1024 * 1024;
const AUTHORIZED_COMMENT_ASSOCIATIONS: ReadonlySet<string> = new Set([
  "OWNER",
  "MEMBER",
  "COLLABORATOR",
]);

export interface GithubEventsDeps {
  webhookSecret?: () => Promise<string>;
  enrollments?: Pick<EnrollmentStore, "get">;
  dispatch?: (input: DispatchReviewInput) => Promise<DispatchReviewResult>;
  /** The review App's @-mention handle (its slug). Defaults to the deployment's
   *  GITHUB_APP_LOGIN; blank disables mention commands. */
  mentionHandle?: string;
}

export function makeGithubEventsRoute(deps: GithubEventsDeps = {}): Hono {
  const webhookSecret = deps.webhookSecret ?? getGithubWebhookSecret;
  const mentionHandle = deps.mentionHandle ?? config.githubAppLogin;
  let enrollmentStore = deps.enrollments;
  const enrollments = (): Pick<EnrollmentStore, "get"> =>
    (enrollmentStore ??= makeEnrollmentStore());
  const dispatch = deps.dispatch ?? ((input) => dispatchReview({
    enrollments: enrollments(),
  }, input));
  const app = new Hono();

  app.post("/api/v1/integrations/github/events", async (c) => {
    const contentLength = c.req.header("content-length");
    if (
      contentLength !== undefined
      && Number(contentLength) > MAX_WEBHOOK_BODY_BYTES
    ) {
      return c.body(null, 413);
    }
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
      // Every remaining pull_request action (opened / synchronize /
      // ready_for_review) is an AUTOMATIC trigger. It fires a review only when
      // the repo is enrolled in auto mode and the PR isn't a draft; in manual
      // (mention-only) mode all of them are ignored — a review comes from an
      // @mention command instead. (synchronize was previously ungated, so a
      // push to a manual-mode PR auto-reviewed — the #802 bug.)
      if (enrollment.triggerMode !== "auto" || event.draft) {
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

    const command = parseReviewCommand(event.body, mentionHandle);
    if (!command) return c.body(null, 200);
    if (event.senderType === "Bot") {
      log.info(
        { repo: event.repo, prNumber: event.prNumber, commentId: event.commentId },
        "github command ignored from bot sender",
      );
      return c.body(null, 200);
    }
    if (!AUTHORIZED_COMMENT_ASSOCIATIONS.has(event.authorAssociation)) {
      log.info(
        {
          repo: event.repo,
          prNumber: event.prNumber,
          commentId: event.commentId,
          authorAssociation: event.authorAssociation,
        },
        "github command ignored from unauthorized commenter",
      );
      return c.body(null, 200);
    }

    if (command.kind === "review") {
      await dispatch({
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: "command",
        idempotencyKey,
        ...(command.focus != null ? { focus: command.focus } : {}),
      });
    } else if (command.kind === "stop") {
      await dispatch({
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: "command",
        idempotencyKey,
        stop: true,
      });
    } else if (command.kind === "fix") {
      log.info(
        { repo: event.repo, prNumber: event.prNumber, commentId: event.commentId },
        "github fix command queued for a later PR",
      );
    }
    return c.body(null, 200);
  });

  return app;
}

export default makeGithubEventsRoute();
