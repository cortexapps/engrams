/** The review engine flip, as ONE transaction (ADR 0119 phase 4.4).
 *
 * A repo's review engine lives in two places that must agree:
 * `review_enrollment.engine` (the GitHub route reads it per delivery) and
 * the PR-review built-in's `repos` input (the dispatcher admits only mapped
 * repos, and only when the built-in is enabled). This module is the ONE
 * writer of that pair, and it writes both inside a single transaction with
 * the two rows locked, so:
 *
 *   - a failure between the writes rolls both back (no row that claims
 *     `automation` while the map lacks it — a repo nothing reviews);
 *   - two concurrent flips for different repos serialize on the automation
 *     row lock, and the map is patched with a JSONB expression in place, so
 *     neither clobbers the other's entry (no lost update).
 *
 * The decision logic (what policy an enrollment maps to, when the built-in
 * turns on) lives beside its primitive here; `reviews/engine-flag.ts` is the
 * caller-facing wrapper that logs and classifies errors.
 *
 * Phase 4.7 deletes `review_enrollment` and this module; the `repos` input
 * becomes the single source.
 */

import { and, eq, isNull, sql } from "drizzle-orm";

import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";
import { getDb } from "./client.ts";
import { automation as automationTable, reviewEnrollment as enrollmentTable } from "./schema.ts";
import type { EnrollmentRow, ReviewEngine } from "./enrollments.ts";

export interface RepoPolicy {
  mode: "auto" | "on_request";
  autofix: boolean;
}

/** What the built-in's `repos` map says about an enrollment. */
export function repoPolicy(row: Pick<EnrollmentRow, "triggerMode" | "autofix">): RepoPolicy {
  return {
    mode: row.triggerMode === "auto" ? "auto" : "on_request",
    autofix: row.autofix !== "off",
  };
}

export type ApplyReviewEngineResult =
  /** The repo is not enrolled; nothing was written. */
  | { kind: "not_enrolled" }
  /** The built-in is not seeded; nothing was written (the row flip rolled back). */
  | { kind: "not_seeded" }
  | {
      kind: "applied";
      enrollment: EnrollmentRow;
      /** Whether the map changed (a `legacy` flip on an unmapped repo does not). */
      builtinUpdated: boolean;
      builtinEnabled: boolean;
    };

export interface ReviewEngineWriter {
  apply(repo: string, engine: ReviewEngine): Promise<ApplyReviewEngineResult>;
}

function toEnrollmentRow(row: typeof enrollmentTable.$inferSelect): EnrollmentRow {
  return {
    repo: row.repo,
    triggerMode: row.triggerMode as EnrollmentRow["triggerMode"],
    autofix: row.autofix as EnrollmentRow["autofix"],
    profileId: row.profileId ?? null,
    engine: row.engine === "automation" ? "automation" : "legacy",
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

export function makeReviewEngineWriter(
  db: ReturnType<typeof getDb> = getDb(),
): ReviewEngineWriter {
  return {
    async apply(repo, engine) {
      return db.transaction(async (tx) => {
        // Lock the enrollment row for the flip, and the automation row so
        // concurrent flips for other repos serialize behind this one.
        const [enrollment] = await tx
          .select()
          .from(enrollmentTable)
          .where(eq(enrollmentTable.repo, repo))
          .for("update");
        if (!enrollment) return { kind: "not_enrolled" as const };

        const [builtin] = await tx
          .select({ id: automationTable.id, enabled: automationTable.enabled, inputs: automationTable.inputs })
          .from(automationTable)
          .where(and(eq(automationTable.builtinKey, PR_REVIEW_BUILTIN_KEY), isNull(automationTable.archivedAt)))
          .for("update");
        if (!builtin) return { kind: "not_seeded" as const };

        const [flipped] = await tx
          .update(enrollmentTable)
          .set({ engine, updatedAt: new Date() })
          .where(eq(enrollmentTable.repo, repo))
          .returning();
        if (!flipped) throw new Error(`review enrollment ${repo} vanished under its lock`);

        const repos = builtin.inputs["repos"];
        const mapped =
          typeof repos === "object" && repos !== null && !Array.isArray(repos) && repo in repos;
        let builtinUpdated = false;
        let builtinEnabled = builtin.enabled;

        if (engine === "automation") {
          // `inputs.repos.<repo> = policy`, in place. The `||` seeds a missing
          // `repos` object first: jsonb_set creates only the LAST path element.
          const policy = JSON.stringify(repoPolicy(toEnrollmentRow(flipped)));
          await tx
            .update(automationTable)
            .set({
              inputs: sql`jsonb_set(
                ${automationTable.inputs} || jsonb_build_object('repos', coalesce(${automationTable.inputs}->'repos', '{}'::jsonb)),
                ARRAY['repos', ${repo}::text],
                ${policy}::jsonb,
                true
              )`,
              ...(builtin.enabled ? {} : { enabled: true }),
              updatedAt: new Date(),
            })
            .where(eq(automationTable.id, builtin.id));
          builtinUpdated = true;
          builtinEnabled = true;
        } else if (mapped) {
          await tx
            .update(automationTable)
            .set({
              inputs: sql`${automationTable.inputs} #- ARRAY['repos', ${repo}::text]`,
              updatedAt: new Date(),
            })
            .where(eq(automationTable.id, builtin.id));
          builtinUpdated = true;
        }

        return {
          kind: "applied" as const,
          enrollment: toEnrollmentRow(flipped),
          builtinUpdated,
          builtinEnabled,
        };
      });
    },
  };
}
