/** Pure GitHub webhook classification and @engrams command parsing (ADR 0100). */

export type GithubEvent =
  | {
      kind: "pull_request";
      action: "opened" | "synchronize" | "ready_for_review" | "closed";
      repo: string;
      prNumber: number;
      headSha: string;
      baseRef: string;
      draft: boolean;
    }
  | {
      kind: "comment";
      repo: string;
      prNumber: number;
      body: string;
      commentId: string;
    }
  | { kind: "ping" }
  | { kind: "ignore" };

export type EngramsCommand =
  | { kind: "review"; focus?: string }
  | { kind: "fix"; text: string }
  | { kind: "stop" };

const PULL_REQUEST_ACTIONS: ReadonlySet<string> = new Set([
  "opened",
  "synchronize",
  "ready_for_review",
  "closed",
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
    const baseRef = base && stringField(base, "ref");
    const draft = pullRequest && booleanField(pullRequest, "draft");
    if (
      !isPullRequestAction(action) ||
      !repo ||
      prNumber == null ||
      !headSha ||
      !baseRef ||
      draft == null
    ) {
      return { kind: "ignore" };
    }
    return {
      kind: "pull_request",
      action,
      repo,
      prNumber,
      headSha,
      baseRef,
      draft,
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

/** Find a command line whose first token is @engrams. Leading prose on earlier
 * lines is allowed; the command itself remains an exact deterministic match. */
export function parseEngramsCommand(body: string): EngramsCommand | null {
  const line = body
    .split(/\r?\n/)
    .map((candidate) => candidate.trim())
    .find((candidate) => /^@engrams(?:\s|$)/i.test(candidate));
  if (!line) return null;

  const match = /^@engrams\s+(review|fix|stop)(?:\s+([\s\S]*))?$/i.exec(line);
  if (!match) return null;
  const verb = match[1]?.toLowerCase();
  const text = sanitize(match[2] ?? "");
  if (verb === "review") {
    return text ? { kind: "review", focus: text } : { kind: "review" };
  }
  if (verb === "fix") return text ? { kind: "fix", text } : null;
  if (verb === "stop") return text ? null : { kind: "stop" };
  return null;
}

function classifyComment(
  body: Record<string, unknown>,
  pr: Record<string, unknown>,
): GithubEvent {
  const repository = recordField(body, "repository");
  const comment = recordField(body, "comment");
  const repo = repository && stringField(repository, "full_name");
  const prNumber = integerField(pr, "number");
  const commentBody = comment && stringField(comment, "body");
  const commentId = comment && identifierField(comment, "id");
  if (!repo || prNumber == null || commentBody == null || commentId == null) {
    return { kind: "ignore" };
  }
  return { kind: "comment", repo, prNumber, body: commentBody, commentId };
}

function sanitize(value: string): string {
  return value
    .replace(/[\u0000-\u001f\u007f-\u009f]/g, "")
    .trim()
    .slice(0, 500);
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
