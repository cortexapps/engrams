/** The review engine flag (ADR 0119 phase 4.4): the caller-facing wrapper
 * over `db/review-engine.ts`, which is the ONE writer of the enrollment
 * flag + built-in `repos` map pair and applies both in a single
 * transaction. This layer classifies the outcomes the product surfaces as
 * errors and logs the reconcile.
 *
 * Phase 4.7 deletes `review_enrollment` and this service; the `repos` input
 * becomes the single source.
 */

import type { ReviewEngineWriter } from "../db/review-engine.ts";
import type { EnrollmentRow, ReviewEngine } from "../db/enrollments.ts";

export type { RepoPolicy } from "../db/review-engine.ts";
export { repoPolicy } from "../db/review-engine.ts";

export interface EngineFlagDeps {
  writer: ReviewEngineWriter;
  log: {
    info(bindings: Record<string, unknown>, message: string): void;
    warn(bindings: Record<string, unknown>, message: string): void;
  };
}

export interface EngineFlagResult {
  enrollment: EnrollmentRow;
  builtinUpdated: boolean;
  builtinEnabled: boolean;
}

/** Flip a repo's review engine and reconcile the built-in's `repos` input,
 * atomically. Throws if the repo is not enrolled, or the built-in is not
 * seeded yet (nothing is written in either case). */
export async function setReviewEngine(
  repo: string,
  engine: ReviewEngine,
  deps: EngineFlagDeps,
): Promise<EngineFlagResult> {
  const result = await deps.writer.apply(repo, engine);
  switch (result.kind) {
    case "not_enrolled":
      throw new EngineFlagError(`repo "${repo}" is not enrolled`);
    case "not_seeded":
      // The seeder runs fire-and-forget at boot; a flip before it lands is an
      // operator error, not a silent no-op.
      throw new EngineFlagError(
        "the PR-review built-in is not seeded yet; retry after the orchestrator finishes booting",
      );
    case "applied": {
      if (result.builtinUpdated) {
        deps.log.info(
          { repo, engine, enabled: result.builtinEnabled },
          "review engine flag: reconciled the built-in repos input",
        );
      }
      return {
        enrollment: result.enrollment,
        builtinUpdated: result.builtinUpdated,
        builtinEnabled: result.builtinEnabled,
      };
    }
  }
}

export class EngineFlagError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "EngineFlagError";
  }
}
