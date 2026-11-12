/** GitHub App webhook deliveries.
 *
 * The event route ledgers each delivery and dispatches it to automations (the
 * PR review built-in among them). This module adds the two things that are
 * not an automation: it keeps the dossier of an already-reviewed PR fresh,
 * and it runs the `stop` mention command.
 */

import { config } from "../../config.ts";
import {
  makeReviewStore,
  type ReviewStore,
  type UpsertReviewTargetInput,
} from "../../db/reviews.ts";
import type { IntegrationEventSource } from "../../automations/integration-ingress.ts";
import { ownPath } from "../../automations/paths.ts";
import { log as rootLog } from "../../log.ts";
import {
  stopAutomationReview,
  type ReviewCoordinate,
  type StopAutomationReviewResult,
} from "../../reviews/automation-review.ts";
import { parseReviewCommand } from "../../forges/review-command.ts";
import { classifyGithubEvent } from "./webhook.ts";

const log = rootLog.child({ component: "github-webhook" });
const AUTHORIZED_COMMENT_ASSOCIATIONS: ReadonlySet<string> = new Set([
  "OWNER",
  "MEMBER",
  "COLLABORATOR",
]);

export interface GithubEventsDeps {
  /** Halt the built-in run behind the PR's active pass. The built-in's own
   *  admission filters the stop comment out, so this reaches the run directly. */
  stopAutomationReview?: (input: ReviewCoordinate) => Promise<StopAutomationReviewResult>;
  /** Refresh a target only when it already exists: a PR action must never
   *  create a dossier for a pull request engrams has not reviewed. */
  refreshTarget?: (input: UpsertReviewTargetInput) => Promise<boolean>;
  /** The review App's @-mention handle (its slug). Blank disables mention commands. */
  mentionHandle?: string;
}

async function refreshExistingTarget(
  reviews: ReviewStore,
  input: UpsertReviewTargetInput,
): Promise<boolean> {
  const existing = await reviews.getTargetForRefresh(input);
  if (!existing) return false;
  if (existing.providerId === null) {
    await reviews.claimTargetId({
      provider: input.provider,
      providerId: input.providerId,
      repo: input.repo,
      number: input.number,
    });
  }
  await reviews.upsertTarget(input);
  return true;
}

const ok = (): Response => new Response(null, { status: 200 });

export function githubEventSource(deps: GithubEventsDeps = {}): IntegrationEventSource {
  const mentionHandle = deps.mentionHandle ?? config.githubAppLogin;
  const stopAutomation = deps.stopAutomationReview ?? stopAutomationReview;
  let reviewStore: ReviewStore | undefined;
  const refreshTarget =
    deps.refreshTarget ??
    ((input) => refreshExistingTarget((reviewStore ??= makeReviewStore()), input));

  return {
    provider: "github",
    // Tolerant (skip, never reject): `afterDelivery` classifies every signed
    // request itself.
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

    afterDelivery: async (delivery, headers) => {
      const eventName = headers.get("x-github-event") ?? "";
      const event = classifyGithubEvent(eventName, new TextDecoder().decode(delivery.rawBody));
      if (event.kind === "ping" || event.kind === "ignore") return ok();

      // Run ids derive from the delivery id, and DBOS dedupes notifications
      // on the message id alone: an empty id would swallow every later send.
      if (!headers.get("x-github-delivery")) {
        log.warn({ event: eventName }, "github delivery carries no delivery id");
        return Response.json({ error: "missing x-github-delivery" }, { status: 400 });
      }

      if (event.kind === "pull_request") {
        if (event.pr.providerId === null) {
          log.warn(
            { repo: event.repo, prNumber: event.prNumber, action: event.action },
            "github PR target refresh skipped because the payload had no provider id",
          );
          return ok();
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
        return ok();
      }

      const command = parseReviewCommand(event.body, mentionHandle);
      if (!command) return ok();
      const where = { repo: event.repo, prNumber: event.prNumber, commentId: event.commentId };
      if (event.senderType === "Bot") {
        log.info(where, "github command ignored from bot sender");
        return ok();
      }
      if (!AUTHORIZED_COMMENT_ASSOCIATIONS.has(event.authorAssociation)) {
        log.info(
          { ...where, authorAssociation: event.authorAssociation },
          "github command ignored from unauthorized commenter",
        );
        return ok();
      }

      switch (command.kind) {
        case "review":
          // The built-in's filter admits the review comment from the dispatch.
          break;
        case "stop": {
          const result = await stopAutomation({
            provider: "github",
            repo: event.repo,
            prNumber: event.prNumber,
          });
          log.info({ ...where, ...result }, "github stop command halted the built-in run");
          break;
        }
        case "fix":
          log.info(where, "github fix command is not supported yet");
          break;
      }
      return ok();
    },
  };
}
