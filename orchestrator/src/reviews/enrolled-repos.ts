/** Which repositories the PR-review built-in reviews: its `repos` input, the
 * single source since ADR 0119 phase 4.7b (the `review_enrollment` table
 * that mirrored it is gone). The Repositories page writes the map through
 * `SetMapInputEntry`; the dispatcher admits a pull request by the same map;
 * this is the read the retry RPC and the CI dispatch edge make before they
 * admit a run for a repo by name. */

import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";
import type { AutomationStore } from "../db/automations.ts";

/** One `repos` entry as the built-in declares it. */
export interface RepoPolicy {
  /** `auto`: every pull request opens a review. `on_request`: only a review
   * comment or a dispatch does. */
  mode: "auto" | "on_request";
  autofix: boolean;
}

export interface EnrolledRepos {
  /** The repo's policy, or null when the built-in does not list it. The
   * match is case-insensitive, as the built-in's own admission is (GitHub
   * repo names are). */
  get(repo: string): Promise<RepoPolicy | null>;
}

export function policyOf(value: unknown): RepoPolicy | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const entry = value as Record<string, unknown>;
  return {
    mode: entry["mode"] === "auto" ? "auto" : "on_request",
    autofix: entry["autofix"] === true,
  };
}

export function makeEnrolledRepos(
  store: Pick<AutomationStore, "getByBuiltinKey">,
): EnrolledRepos {
  return {
    async get(repo) {
      const builtin = await store.getByBuiltinKey(PR_REVIEW_BUILTIN_KEY);
      if (!builtin || builtin.archivedAt !== null) return null;
      const repos = builtin.inputs["repos"];
      if (typeof repos !== "object" || repos === null || Array.isArray(repos)) return null;
      const wanted = repo.toLowerCase();
      const hit = Object.entries(repos as Record<string, unknown>).find(
        ([key]) => key.toLowerCase() === wanted,
      );
      return hit ? policyOf(hit[1]) : null;
    },
  };
}
