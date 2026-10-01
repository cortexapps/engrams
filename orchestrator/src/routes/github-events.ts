/** GitHub App webhook edge: verify, classify, and durably dispatch (ADR 0100). */

import { Hono } from "hono";

import { config } from "../config.ts";
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
import {
  handleIntegrationDelivery,
  type HandleDeliveryDeps,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import { ownPath } from "../automations/paths.ts";
import { log as rootLog } from "../log.ts";
import {
  stopAutomationReview,
  type ReviewCoordinate,
  type StopAutomationReviewResult,
} from "../reviews/automation-review.ts";

const log = rootLog.child({ component: "github-webhook" });
const AUTHORIZED_COMMENT_ASSOCIATIONS: ReadonlySet<string> = new Set([
  "OWNER",
  "MEMBER",
  "COLLABORATOR",
]);

export interface GithubEventsDeps {
  webhookSecret?: () => Promise<string>;
  /** `@engrams stop`: halt the built-in run behind the PR's active pass (the
   *  built-in's own admission filters the stop comment out, so the route
   *  must reach the run directly). */
  stopAutomationReview?: (input: ReviewCoordinate) => Promise<StopAutomationReviewResult>;
  /** Refresh a target only when it already exists. Non-reviewing PR actions
   *  must never create dossiers for pull requests engrams has never reviewed. */
  refreshTarget?: (input: UpsertReviewTargetInput) => Promise<boolean>;
  /** Ingress-spine seams (ledger store, new-trigger dispatch, connection). */
  ingress?: HandleDeliveryDeps;
  now?: () => Date;
  /** The review App's @-mention handle (its slug). Defaults to the deployment's
   *  GITHUB_APP_LOGIN; blank disables mention commands. */
  mentionHandle?: string;
}

export function makeGithubEventsRoute(deps: GithubEventsDeps = {}): Hono {
  const webhookSecret = deps.webhookSecret ?? getGithubWebhookSecret;
  const mentionHandle = deps.mentionHandle ?? config.githubAppLogin;
  const stopAutomation = deps.stopAutomationReview ?? stopAutomationReview;
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
  const now = deps.now ?? (() => new Date());
  const ingressDeps: HandleDeliveryDeps = { ...(deps.ingress ?? {}), now };

  // The ingress spine (ADR 0119 D5): verify → ledger → new-trigger dispatch.
  // Verification is the same App-webhook-secret HMAC as before; extraction is
  // deliberately tolerant (skip, never reject) so the PR-review classifier
  // below keeps its exact behavior for every signed request.
  const ingressRoute: IntegrationEventRoute = {
    provider: "github",
    displayName: "GitHub (default)",
    verify: async (headers, rawBody) =>
      verifyGithubSignature(
        await webhookSecret(),
        rawBody,
        headers.get("x-hub-signature-256") ?? undefined,
      ),
    extract: (headers, payload) => {
      const base = headers.get("x-github-event");
      if (!base) return { kind: "skip", reason: "missing x-github-event" };
      const action = payload["action"];
      if (action !== undefined && (typeof action !== "string" || action === "")) {
        return { kind: "skip", reason: "non-string payload action" };
      }
      const deliveryId = headers.get("x-github-delivery");
      if (!deliveryId) return { kind: "skip", reason: "missing x-github-delivery" };
      const repo = ownPath(payload, "repository.full_name");
      return {
        kind: "event",
        event: {
          eventKey: action === undefined ? base : `${base}.${action}`,
          deliveryId,
          ...(typeof repo === "string" ? { scopeValue: repo.toLowerCase() } : {}),
        },
      };
    },
  };

  const app = new Hono();

  app.post("/api/v1/integrations/github/events", async (c) => {
    const delivery = await handleIntegrationDelivery(ingressRoute, c.req.raw, ingressDeps);
    if (delivery.kind === "rejected" || delivery.kind === "responded") {
      return delivery.response;
    }
    // The spine (ledger + integration-trigger dispatch) has run; the PR-review
    // classifier below consumes the same verified body.
    const rawBody = new TextDecoder().decode(delivery.rawBody);

    const event = classifyGithubEvent(
      c.req.header("x-github-event") ?? "",
      rawBody,
    );
    if (event.kind === "ping") return c.body(null, 200);
    if (event.kind === "ignore") return c.body(null, 200);

    // `X-GitHub-Delivery` is part of GitHub's documented webhook contract, and the
    // signature check above already passed, so a request without one is malformed
    // rather than merely unusual. Refuse it instead of inventing a key: there is
    // nothing stable to deduplicate a redelivery against, and an empty key is
    // actively destructive downstream — DBOS takes it as a real message id and its
    // notifications table conflicts on that id ALONE, so the first empty-key send
    // silently swallows every later one, for every review, until the row is
    // deleted by hand.
    const idempotencyKey = c.req.header("x-github-delivery");
    if (!idempotencyKey) {
      log.warn(
        { event: c.req.header("x-github-event") ?? "" },
        "github delivery carries no delivery id",
      );
      return c.json({ error: "missing x-github-delivery" }, 400);
    }

    if (event.kind === "pull_request") {
      // The spine dispatched the delivery to the PR-review built-in already;
      // its admission decides whether this action starts a pass. The route
      // only keeps the dossier target fresh for a PR engrams has reviewed.
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
        { repo: event.repo, prNumber: event.prNumber, action: event.action, refreshed },
        "github PR action refreshed the review target",
      );
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
      // The @mention delivery was ledgered and dispatched to the built-in,
      // whose filter admits the `^@engrams review` comment. Nothing to start.
      log.info(
        { repo: event.repo, prNumber: event.prNumber },
        "github review command dispatched to the built-in by the spine",
      );
    } else if (command.kind === "stop") {
      const result = await stopAutomation({ repo: event.repo, prNumber: event.prNumber });
      log.info(
        { repo: event.repo, prNumber: event.prNumber, ...result },
        "github stop command halted the built-in run",
      );
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
