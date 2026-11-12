/** GitHub's pull-request object, read without I/O. */

import type { PrContext } from "../../forges/pr-context.ts";

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * GitHub's pull-request object as a `PrContext`. The same object is the
 * `GET /repos/{repo}/pulls/{n}` response and the webhook payload's
 * `pull_request` member. Every field falls back to null on its own. The two
 * SHAs are not read here: callers require them.
 */
export function readPrContext(pr: Record<string, unknown>): PrContext {
  const head = isObject(pr["head"]) ? pr["head"] : null;
  const base = isObject(pr["base"]) ? pr["base"] : null;
  const user = isObject(pr["user"]) ? pr["user"] : null;
  return {
    providerId: identifier(pr["id"]),
    title: nullableString(pr["title"]),
    author: user ? nullableString(user["login"]) : null,
    state: readPrState(pr),
    url: nullableString(pr["html_url"]),
    providerUpdatedAt: nullableDate(pr["updated_at"]),
    headBranch: head ? nullableString(head["ref"]) : null,
    baseBranch: base ? nullableString(base["ref"]) : null,
    additions: nullableCount(pr["additions"]),
    deletions: nullableCount(pr["deletions"]),
    changedFiles: nullableCount(pr["changed_files"]),
  };
}

/** GitHub gives `state` as open or closed, with `merged` and `draft` apart.
 * Merged outranks closed; draft applies only while open. */
function readPrState(pr: Record<string, unknown>): string | null {
  if (pr["merged"] === true) return "merged";
  const state = nullableString(pr["state"]);
  if (state === "open") return pr["draft"] === true ? "draft" : "open";
  return state;
}

/** GitHub ids arrive as JSON numbers; they are carried as strings so a large
 * one cannot round. */
function identifier(value: unknown): string | null {
  if (typeof value === "string" && value !== "") return value;
  if (typeof value === "number" && Number.isSafeInteger(value) && value > 0) {
    return String(value);
  }
  return null;
}

function nullableDate(value: unknown): Date | null {
  if (typeof value !== "string" || value === "") return null;
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function nullableString(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function nullableCount(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}
