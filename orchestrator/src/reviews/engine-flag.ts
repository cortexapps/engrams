/** The review engine flag ↔ built-in coherence service (ADR 0119 phase 4.4).
 *
 * During the parallel window a repo's review engine is chosen by
 * `review_enrollment.engine`, but the PR-review built-in only reviews repos
 * present in its `repos` input (and only when the built-in is enabled). Those
 * are two sources that must agree, so flipping the flag is the ONE write path
 * that keeps them in sync:
 *
 *   → automation: upsert the repo into the built-in's `repos` map (mode/autofix
 *     derived from the enrollment) and enable the built-in if it was disabled.
 *   → legacy:     remove the repo from the map. The built-in stays enabled (it
 *     may still own other repos); an operator disables it explicitly as the
 *     second, independent brake.
 *
 * Phase 4.7 deletes `review_enrollment` and this service; the `repos` input
 * becomes the single source.
 */

import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";
import type { AutomationRow } from "../db/automations.ts";
import type { EnrollmentRow, ReviewEngine } from "../db/enrollments.ts";

export interface EngineFlagStore {
  getByBuiltinKey(key: string): Promise<AutomationRow | null>;
  setInputs(id: string, inputs: Record<string, unknown>): Promise<AutomationRow | null>;
  setEnabled(id: string, enabled: boolean): Promise<AutomationRow | null>;
}

export interface EngineFlagDeps {
  setEnrollmentEngine(repo: string, engine: ReviewEngine): Promise<EnrollmentRow | null>;
  builtins: EngineFlagStore;
  log: {
    info(bindings: Record<string, unknown>, message: string): void;
    warn(bindings: Record<string, unknown>, message: string): void;
  };
}

interface RepoPolicy {
  mode: "auto" | "on_request";
  autofix: boolean;
}

function repoPolicy(enrollment: EnrollmentRow): RepoPolicy {
  return {
    mode: enrollment.triggerMode === "auto" ? "auto" : "on_request",
    autofix: enrollment.autofix !== "off",
  };
}

function reposMap(row: AutomationRow): Record<string, RepoPolicy> {
  const value = row.inputs["repos"];
  if (typeof value !== "object" || value === null || Array.isArray(value)) return {};
  // Shallow copy so we never mutate the stored object in place.
  return { ...(value as Record<string, RepoPolicy>) };
}

export interface EngineFlagResult {
  enrollment: EnrollmentRow;
  builtinUpdated: boolean;
  builtinEnabled: boolean;
}

/** Flip a repo's review engine and reconcile the built-in's `repos` input.
 * Throws if the repo is not enrolled, or the built-in is not seeded yet. */
export async function setReviewEngine(
  repo: string,
  engine: ReviewEngine,
  deps: EngineFlagDeps,
): Promise<EngineFlagResult> {
  const enrollment = await deps.setEnrollmentEngine(repo, engine);
  if (!enrollment) {
    throw new EngineFlagError(`repo "${repo}" is not enrolled`);
  }

  const builtin = await deps.builtins.getByBuiltinKey(PR_REVIEW_BUILTIN_KEY);
  if (!builtin) {
    // The seeder runs fire-and-forget at boot; a flip before it lands is an
    // operator error, not a silent no-op.
    throw new EngineFlagError(
      "the PR-review built-in is not seeded yet; retry after the orchestrator finishes booting",
    );
  }

  const repos = reposMap(builtin);
  const present = repo in repos;
  let builtinUpdated = false;

  if (engine === "automation") {
    repos[repo] = repoPolicy(enrollment);
    builtinUpdated = true;
  } else if (present) {
    delete repos[repo];
    builtinUpdated = true;
  }

  let builtinEnabled = builtin.enabled;
  if (builtinUpdated) {
    await deps.builtins.setInputs(builtin.id, { ...builtin.inputs, repos });
    deps.log.info(
      { repo, engine, builtinId: builtin.id },
      "review engine flag: reconciled the built-in repos input",
    );
    if (engine === "automation" && !builtin.enabled) {
      await deps.builtins.setEnabled(builtin.id, true);
      builtinEnabled = true;
      deps.log.info(
        { repo, builtinId: builtin.id },
        "review engine flag: enabled the PR-review built-in for its first flagged repo",
      );
    }
  }

  return { enrollment, builtinUpdated, builtinEnabled };
}

export class EngineFlagError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "EngineFlagError";
  }
}
