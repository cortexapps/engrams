/** Admission of a forge event to the code-review built-in, and the facts the
 * rest of the graph reads. The forge says what the event is (`reviewEvent`);
 * this module holds the policy, which is the same for every forge. */

import type { GitForge, ReviewEvent } from "./forge.ts";
import type { PrContext } from "./pr-context.ts";
import { authorizedCommand } from "./review-command.ts";

/** The value of the built-in's `facts` step. */
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
  /** `providerUpdatedAt` is ISO 8601: a step's value is JSON. */
  pr_context: Omit<PrContext, "providerUpdatedAt"> & { providerUpdatedAt: string | null };
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

/** The entry of `repo` in a `repos` input; null when the map does not list
 * the repository. */
export function enrolledEntry(forge: GitForge, repos: unknown, repo: string): unknown {
  if (typeof repos !== "object" || repos === null || Array.isArray(repos)) return null;
  const wanted = forge.repoKey(repo);
  return Object.entries(repos).find(([name]) => forge.repoKey(name) === wanted)?.[1] ?? null;
}

/** The trigger of an event that starts a review; null when it starts none. */
async function reviewTrigger(
  forge: GitForge,
  event: ReviewEvent,
  mode: unknown,
  mention: unknown,
): Promise<ReviewFacts["trigger"] | null> {
  switch (event.kind) {
    case "change":
      return event.draft || mode !== "auto" ? null : event.trigger;
    case "comment": {
      const handle = typeof mention === "string" ? mention : "";
      return (await authorizedCommand(forge, event, handle, ["review"])) ? "command" : null;
    }
    case "dispatch":
      // An explicit request: it does not need mode "auto".
      return "dispatch";
    case "update":
      return null;
  }
}

/** The facts of an admitted event; null when the event starts no review. */
export async function reviewFacts(
  forge: GitForge,
  input: ReviewFactsInput,
): Promise<ReviewFacts | null> {
  const event = forge.reviewEvent(input.eventKey, input.payload);
  if (event === null) return null;
  const { inputs } = input;
  const entry = enrolledEntry(forge, inputs["repos"], event.pr.repo);
  if (typeof entry !== "object" || entry === null) return null;
  // With no reviewer profile nothing can run, so the event is filtered.
  if (!inputs["profile"]) return null;
  const trigger = await reviewTrigger(
    forge,
    event,
    (entry as { mode?: unknown }).mode,
    inputs["mention"],
  );
  if (trigger === null) return null;

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
    pr_context: {
      ...pr.context,
      providerUpdatedAt: pr.context.providerUpdatedAt?.toISOString() ?? null,
    },
    categories: Array.isArray(inputs["categories"]) ? inputs["categories"] : [],
    instructions: typeof inputs["instructions"] === "string" ? inputs["instructions"] : "",
  };
}
