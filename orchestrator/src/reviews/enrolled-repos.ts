/** Which repositories a forge's code-review built-in reviews: its `repos` input, the
 * single source since ADR 0119 phase 4.7b (the `review_enrollment` table
 * that mirrored it is gone). The Repositories page writes the map through
 * `SetMapInputEntry`; the dispatcher admits a pull request by the same map;
 * this is the read the retry RPC and the CI dispatch edge make before they
 * admit a run for a repo by name. */

import { forgeFor, reviewVariantFor } from "../forges/registry.ts";
import { enrolledEntry } from "../forges/review-facts.ts";
import type { AutomationStore } from "../db/automations.ts";

/** One `repos` entry as the built-in declares it. */
export interface RepoPolicy {
  /** `auto`: every pull request opens a review. `on_request`: only a review
   * comment or a dispatch does. */
  mode: "auto" | "on_request";
  autofix: boolean;
}

export interface EnrolledRepos {
  /** The repo's policy in the forge's code-review built-in, or null when
   * the built-in does not list it. */
  get(provider: string, repo: string): Promise<RepoPolicy | null>;
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
    async get(provider, repo) {
      const forge = forgeFor(provider);
      const variant = reviewVariantFor(provider);
      if (forge === null || variant === null) return null;
      const builtin = await store.getByBuiltinKey(variant.key);
      if (!builtin || builtin.archivedAt !== null) return null;
      return policyOf(enrolledEntry(forge, builtin.inputs["repos"], repo));
    },
  };
}
