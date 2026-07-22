/** GitHub App webhook edge: verify, classify, and durably dispatch (ADR 0100). */

import { Hono } from "hono";

import {
  dispatchWebhookOccurrence,
  SYSTEM_GITHUB_REGISTRATION_ID,
  type DispatchWebhookInput,
} from "../automations/dispatch.ts";
import {
  extractWebhookEvent,
  parseWebhookPayload,
  redactWebhookPayload,
  WebhookEventError,
} from "../automations/webhook.ts";
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
import { BodyTooLargeError, readBoundedBody } from "../http/bounded-body.ts";
import { log as rootLog } from "../log.ts";
import {
  dispatchReview,
  type DispatchReviewInput,
  type DispatchReviewResult,
} from "../workflows/dispatch-review.ts";

const log = rootLog.child({ component: "github-webhook" });
const AUTHORIZED_COMMENT_ASSOCIATIONS: ReadonlySet<string> = new Set([
  "OWNER",
  "MEMBER",
  "COLLABORATOR",
]);

export interface GithubEventsDeps {
  webhookSecret?: () => Promise<string>;
  enrollments?: Pick<EnrollmentStore, "get">;
  dispatch?: (input: DispatchReviewInput) => Promise<DispatchReviewResult>;
  automationDispatch?: (input: DispatchWebhookInput) => Promise<unknown>;
  now?: () => Date;
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
  const automationDispatch = deps.automationDispatch ?? dispatchWebhookOccurrence;
  const now = deps.now ?? (() => new Date());
  const app = new Hono();

  app.post("/api/v1/integrations/github/events", async (c) => {
    let rawBytes: Uint8Array;
    try {
      rawBytes = await readBoundedBody(c.req.raw);
    } catch (error) {
      if (error instanceof BodyTooLargeError) return c.body(null, 413);
      throw error;
    }
    const valid = verifyGithubSignature(
      await webhookSecret(),
      rawBytes,
      c.req.header("x-hub-signature-256"),
    );
    if (!valid) return c.json({ error: "invalid signature" }, 401);

    const rawBody = new TextDecoder().decode(rawBytes);
    // The installed GitHub App is a well-known system registration. It has no
    // PG registration row, so dispatch skips sample persistence but still
    // matches automations bound to "github-app". This happens before the
    // PR-review classifier so every verified GitHub event is forwarded.
    try {
      const occurrence = extractWebhookEvent({
        registration: {
          verification: { scheme: "github_hmac_sha256", secretRef: "github.webhook_secret" },
          providerHint: "github",
        },
        headers: c.req.raw.headers,
        rawBody: rawBytes,
        payload: parseWebhookPayload(rawBytes),
      });
      await automationDispatch({
        registrationId: SYSTEM_GITHUB_REGISTRATION_ID,
        registration: null,
        eventKey: occurrence.eventKey,
        deliveryId: occurrence.deliveryId,
        payload: redactWebhookPayload(occurrence.payload),
        receivedAt: now(),
      });
    } catch (error) {
      if (!(error instanceof WebhookEventError)) throw error;
      // Preserve the pre-existing PR-review classifier's tolerant behavior for
      // signed-but-malformed/non-JSON requests. Legitimate GitHub deliveries
      // always carry the event and delivery headers and a JSON object body.
      log.warn({ error: error.message }, "github delivery not eligible for automation dispatch");
    }

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
