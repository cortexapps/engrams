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
  makeReviewStore,
  type ReviewStore,
  type UpsertReviewTargetInput,
} from "../db/reviews.ts";
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
import {
  startReviewIngress,
  type ReviewIngressStart,
} from "../workflows/review-ingress.ts";

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
  /** Starts the durable ingress workflow that resolves the change and then starts
   *  a review (ADR 0100 d11). Separate from `dispatch`, which carries comments
   *  and stops straight to a running pass and needs no resolution. */
  startIngress?: (input: ReviewIngressStart) => Promise<void>;
  /** Refresh a target only when it already exists. Non-reviewing PR actions
   *  must never create dossiers for pull requests engrams has never reviewed. */
  refreshTarget?: (input: UpsertReviewTargetInput) => Promise<boolean>;
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
  const startIngress = deps.startIngress ?? startReviewIngress;
  let reviewStore: ReviewStore | undefined;
  const reviews = (): ReviewStore => (reviewStore ??= makeReviewStore());
  const refreshTarget = deps.refreshTarget ?? (async (input) => {
    const existing = await reviews().getTargetForRefresh(input);
    if (!existing) return false;
    if (existing.providerId === null) {
      await reviews().claimTargetId({
        provider: input.provider,
        providerId: input.providerId,
        repo: input.repo,
        number: input.number,
      });
    }
    await reviews().upsertTarget(input);
    return true;
  });
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

    const idempotencyKey = c.req.header("x-github-delivery") ?? "";

    if (event.kind === "pull_request") {
      const enrollment = await enrollments().get(event.repo);
      const startsReview = (
        event.action === "opened"
        || event.action === "synchronize"
        || event.action === "ready_for_review"
      ) && enrollment?.triggerMode === "auto" && !event.draft;

      if (!startsReview) {
        if (event.pr.providerId === null) {
          log.warn(
            { repo: event.repo, prNumber: event.prNumber, action: event.action },
            "github PR target refresh skipped because the payload had no provider id",
          );
          return c.body(null, 200);
        }
        const refreshed = await refreshTarget({
          provider: "github",
          providerId: event.pr.providerId,
          repo: event.repo,
          number: event.prNumber,
          title: event.pr.title,
          author: event.pr.author,
          state: event.pr.state,
          url: event.pr.url,
          providerUpdatedAt: event.pr.providerUpdatedAt,
        });
        log.info(
          {
            repo: event.repo,
            prNumber: event.prNumber,
            action: event.action,
            refreshed,
          },
          "github PR action refreshed target without starting a review",
        );
        return c.body(null, 200);
      }
      // The delivery already describes the change in full, so ingress starts a
      // review without asking GitHub anything (ADR 0100 decision 11).
      await startIngress({
        provider: "github",
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: event.action === "synchronize" ? "synchronize" : "opened",
        idempotencyKey,
        headSha: event.headSha,
        ...(event.baseSha != null ? { baseSha: event.baseSha } : {}),
        pr: event.pr,
      });
      return c.body(null, 200);
    }

    const enrollment = await enrollments().get(event.repo);
    if (!enrollment) return c.body(null, 200);
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
      // An issue_comment payload describes the ISSUE, and `issue.id` is the
      // issue's id, not the pull request's. So ingress must resolve this one.
      await startIngress({
        provider: "github",
        repo: event.repo,
        prNumber: event.prNumber,
        trigger: "command",
        idempotencyKey,
        commentId: event.commentId,
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
