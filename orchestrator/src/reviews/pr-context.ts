/**
 * One reader for GitHub's pull-request object (ADR 0100 decision 11).
 *
 * The same shape arrives by two routes — the `GET /repos/{repo}/pulls/{n}`
 * response and the `pull_request` webhook payload's `pull_request` member — and
 * both are parsed here so a field can never be understood one way on the fetch
 * path and another on the webhook path.
 *
 * Deliberately total: every field falls back to null on its own, so an
 * unexpected payload costs a title, never a review. The two SHAs are the
 * exception — they are load-bearing, so callers extract and validate those
 * themselves rather than reading a nullable here.
 */

/**
 * A pull request as GitHub last described it.
 *
 * The fields split across two tables downstream, and the split is not cosmetic:
 *
 *   IDENTITY (`review_target`, one row per PR, refreshed on every capture) —
 *   `providerId`, `title`, `author`, `state`, `url`. These describe the pull
 *   request itself, so the freshest capture is the truth.
 *
 *   PER-PASS (`review`, one row per pass, never rewritten) — `headBranch`,
 *   `baseBranch`, `additions`, `deletions`, `changedFiles`. These describe the
 *   code THIS pass read: the diff grows with every push and a PR can be
 *   retargeted, so overwriting them would make an old pass misreport what it
 *   actually reviewed.
 */
export interface PrContext {
  /** The forge's stable id — the durable identity, and the only field a rename
   *  or a transfer cannot move. `repo` is a mutable, reusable name. */
  providerId: string | null;
  title: string | null;
  /** Login of the PR author. */
  author: string | null;
  /** open | draft | closed | merged, as of this capture. */
  state: string | null;
  /** The forge's own URL, so a link never has to be rebuilt from a repo name
   *  that may since have changed. */
  url: string | null;
  /** The forge's own last-modified time. Deliveries are not ordered, so this is
   *  what lets a late one be recognised as stale. */
  providerUpdatedAt: Date | null;
  headBranch: string | null;
  baseBranch: string | null;
  additions: number | null;
  deletions: number | null;
  changedFiles: number | null;
}

/** Pull the descriptive fields out of a GitHub pull-request object. */
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

/**
 * True when a capture carries every field we persist, so the API fetch adds
 * nothing and can be skipped.
 *
 * The list below is exhaustive over `PrContext` on purpose — this is not "enough
 * to proceed", it is "nothing left to learn". Checked against the DATA rather
 * than against a list of webhook actions we believe are complete: a wrong belief
 * about an action would be invisible, because it would silently persist nulls
 * instead of raising anything. Being wrong here costs one API call.
 */
export function isCompletePrContext(pr: PrContext): boolean {
  return (
    pr.providerId !== null &&
    pr.title !== null &&
    pr.author !== null &&
    pr.state !== null &&
    pr.url !== null &&
    pr.providerUpdatedAt !== null &&
    pr.headBranch !== null &&
    pr.baseBranch !== null &&
    pr.additions !== null &&
    pr.deletions !== null &&
    pr.changedFiles !== null
  );
}

/**
 * GitHub splits a PR's disposition across three fields — `state` is only
 * open/closed, with `merged` and `draft` as separate booleans. Readers think in
 * one axis, so fold them: merged outranks closed (every merged PR is closed),
 * and draft only means anything while the PR is open.
 */
function readPrState(pr: Record<string, unknown>): string | null {
  if (pr["merged"] === true) return "merged";
  const state = nullableString(pr["state"]);
  if (state === "open") return pr["draft"] === true ? "draft" : "open";
  return state;
}

/** GitHub ids exceed 2^53 in principle and arrive as JSON numbers, so they are
 *  carried as strings — never as a number that could round. */
function identifier(value: unknown): string | null {
  if (typeof value === "string" && value !== "") return value;
  if (typeof value === "number" && Number.isSafeInteger(value) && value > 0) {
    return String(value);
  }
  return null;
}

/** An ISO-8601 timestamp, or null when it is absent or unparseable. Never throws
 *  and never yields an Invalid Date, which would poison every later comparison. */
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

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
