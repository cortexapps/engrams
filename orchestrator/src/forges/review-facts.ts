/** Admission of a forge event to the code-review built-in, and the facts the
 * rest of the graph reads. The forge says what the event is (`reviewEvent`);
 * this module holds the policy, which is the same for every forge. */

import type { GitForge, ReviewEvent } from "./forge.ts";
import { parseReviewCommand } from "./review-command.ts";

/** The value of a review variant's `facts` step. */
export interface ReviewFacts {
  admit: true;
  repo: string;
  /** The directory name of the checkout: the last name of the path. */
  repo_name: string;
  pr_number: number;
  pr_url: string;
  trigger: "opened" | "synchronize" | "ready_for_review" | "command" | "dispatch";
  /** Empty when the event carried none; the pass then asks the forge. */
  head_sha: string;
  base_sha: string;
  pr_context: ReviewEvent["pr"]["context"];
  categories: unknown[];
  instructions: string;
}

export interface ReviewFactsInput {
  eventKey: string;
  payload: Record<string, unknown>;
  /** The built-in's inputs: `repos`, `profile`, `mention`, `categories`,
   * `instructions`. */
  inputs: Record<string, unknown>;
}

function enrolment(
  forge: GitForge,
  inputs: Record<string, unknown>,
  repo: string,
): { mode?: unknown } | null {
  const repos = inputs["repos"];
  if (typeof repos !== "object" || repos === null || Array.isArray(repos)) return null;
  const fold = (name: string): string => (forge.caseInsensitiveRepos ? name.toLowerCase() : name);
  const wanted = fold(repo);
  const entry = Object.entries(repos).find(([name]) => fold(name) === wanted)?.[1];
  return typeof entry === "object" && entry !== null ? (entry as { mode?: unknown }) : null;
}

/** The facts of an admitted event; null when the event starts no review. */
export async function reviewFacts(
  forge: GitForge,
  input: ReviewFactsInput,
): Promise<ReviewFacts | null> {
  if (!forge.reviewEvent) {
    throw new Error(`forge "${forge.provider}" does not classify review events`);
  }
  const event = forge.reviewEvent(input.eventKey, input.payload);
  if (event === null) return null;
  const { inputs } = input;
  const entry = enrolment(forge, inputs, event.pr.repo);
  if (entry === null) return null;
  // With no reviewer profile nothing can run, so the event is filtered.
  if (!inputs["profile"]) return null;

  let trigger: ReviewFacts["trigger"];
  switch (event.kind) {
    case "change":
      if (event.draft || entry.mode !== "auto") return null;
      trigger = event.trigger;
      break;
    case "comment": {
      if (event.fromBot) return null;
      const mention = typeof inputs["mention"] === "string" ? inputs["mention"] : "";
      if (parseReviewCommand(event.body, mention)?.kind !== "review") return null;
      if (!(await forge.mayRequestReview?.(event.pr.repo, event.authorId))) return null;
      trigger = "command";
      break;
    }
    case "dispatch":
      // An explicit request: it does not need mode "auto".
      trigger = "dispatch";
      break;
  }

  const { pr } = event;
  return {
    admit: true,
    repo: pr.repo,
    repo_name: pr.repo.split("/").at(-1) ?? "",
    pr_number: pr.number,
    pr_url: pr.url,
    trigger,
    head_sha: pr.headSha,
    base_sha: pr.baseSha,
    pr_context: pr.context,
    categories: Array.isArray(inputs["categories"]) ? inputs["categories"] : [],
    instructions: typeof inputs["instructions"] === "string" ? inputs["instructions"] : "",
  };
}
