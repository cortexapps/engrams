/** GitHub webhook deliveries, classified without I/O. */

import type { PrContext } from "../../forges/pr-context.ts";
import { readPrContext } from "./pr.ts";

export type GithubEvent =
  | {
      kind: "pull_request";
      action:
        | "opened"
        | "synchronize"
        | "ready_for_review"
        | "closed"
        | "reopened"
        | "edited"
        | "converted_to_draft";
      repo: string;
      prNumber: number;
      headSha: string;
      /** Null when the payload omitted it; the pass then resolves heads the
       *  slow way rather than being dropped. */
      baseSha: string | null;
      baseRef: string;
      draft: boolean;
      /** The pull request as this delivery described it. The payload carries
       *  the whole object, so a webhook-triggered pass needs no API call to
       *  name what it reviews (ADR 0100 decision 11). */
      pr: PrContext;
    }
  | {
      kind: "comment";
      repo: string;
      prNumber: number;
      body: string;
      commentId: string;
      authorAssociation: string;
      senderType: string;
    }
  | { kind: "ping" }
  | { kind: "ignore" };

const PULL_REQUEST_ACTIONS: ReadonlySet<string> = new Set([
  "opened",
  "synchronize",
  "ready_for_review",
  "closed",
  "reopened",
  "edited",
  "converted_to_draft",
] as const);

type PullRequestAction = Extract<GithubEvent, { kind: "pull_request" }>["action"];

export function classifyGithubEvent(
  eventName: string,
  rawBody: string,
): GithubEvent {
  let body: unknown;
  try {
    body = JSON.parse(rawBody);
  } catch {
    return { kind: "ignore" };
  }
  if (eventName === "ping") return { kind: "ping" };
  if (!isRecord(body)) return { kind: "ignore" };

  if (eventName === "pull_request") {
    const action = stringField(body, "action");
    const repository = recordField(body, "repository");
    const pullRequest = recordField(body, "pull_request");
    const head = pullRequest && recordField(pullRequest, "head");
    const base = pullRequest && recordField(pullRequest, "base");
    const repo = repository && stringField(repository, "full_name");
    const prNumber = integerField(body, "number");
    const headSha = head && stringField(head, "sha");
    const baseSha = base && stringField(base, "sha");
    const baseRef = base && stringField(base, "ref");
    const draft = pullRequest && booleanField(pullRequest, "draft");
    // `baseSha` is deliberately NOT required here. It is an optimisation — its
    // absence costs the pass one API call, whereas gating classification on it
    // would silently drop a real review trigger. `pullRequest` is implied by
    // `headSha` (which is read through it) but TypeScript cannot see that.
    if (
      !isPullRequestAction(action) ||
      !repo ||
      prNumber == null ||
      !headSha ||
      !baseRef ||
      draft == null ||
      !pullRequest
    ) {
      return { kind: "ignore" };
    }
    return {
      kind: "pull_request",
      action,
      repo,
      prNumber,
      headSha,
      baseSha,
      baseRef,
      draft,
      pr: readPrContext(pullRequest),
    };
  }

  if (eventName === "issue_comment") {
    if (stringField(body, "action") !== "created") return { kind: "ignore" };
    const issue = recordField(body, "issue");
    if (!issue || !recordField(issue, "pull_request")) return { kind: "ignore" };
    return classifyComment(body, issue);
  }

  if (eventName === "pull_request_review_comment") {
    if (stringField(body, "action") !== "created") return { kind: "ignore" };
    const pullRequest = recordField(body, "pull_request");
    if (!pullRequest) return { kind: "ignore" };
    return classifyComment(body, pullRequest);
  }

  return { kind: "ignore" };
}

function classifyComment(
  body: Record<string, unknown>,
  pr: Record<string, unknown>,
): GithubEvent {
  const repository = recordField(body, "repository");
  const comment = recordField(body, "comment");
  const sender = recordField(body, "sender");
  const repo = repository && stringField(repository, "full_name");
  const prNumber = integerField(pr, "number");
  const commentBody = comment && stringField(comment, "body");
  const commentId = comment && identifierField(comment, "id");
  const authorAssociation = comment && stringField(comment, "author_association");
  const senderType = sender && stringField(sender, "type");
  if (
    !repo
    || prNumber == null
    || commentBody == null
    || commentId == null
    || authorAssociation == null
    || senderType == null
  ) {
    return { kind: "ignore" };
  }
  return {
    kind: "comment",
    repo,
    prNumber,
    body: commentBody,
    commentId,
    authorAssociation,
    senderType,
  };
}

function isPullRequestAction(value: string | null): value is PullRequestAction {
  return value != null && PULL_REQUEST_ACTIONS.has(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function recordField(
  value: Record<string, unknown>,
  key: string,
): Record<string, unknown> | null {
  const field = value[key];
  return isRecord(field) ? field : null;
}

function stringField(value: Record<string, unknown>, key: string): string | null {
  const field = value[key];
  return typeof field === "string" ? field : null;
}

function integerField(value: Record<string, unknown>, key: string): number | null {
  const field = value[key];
  return typeof field === "number" && Number.isSafeInteger(field) && field > 0
    ? field
    : null;
}

function booleanField(value: Record<string, unknown>, key: string): boolean | null {
  const field = value[key];
  return typeof field === "boolean" ? field : null;
}

function identifierField(value: Record<string, unknown>, key: string): string | null {
  const field = value[key];
  if (typeof field === "string" && field) return field;
  if (typeof field === "number" && Number.isSafeInteger(field)) return String(field);
  return null;
}
