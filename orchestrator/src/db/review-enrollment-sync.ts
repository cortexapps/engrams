/** The enrollment reconcile, as ONE transaction (ADR 0119 phase 4.4; the
 * engine flag itself retired in phase 4.7).
 *
 * A repo's review enrollment lives in two places that must agree:
 * `review_enrollment` (the Repositories page's row) and the PR-review
 * built-in's `repos` input (the dispatcher admits only mapped repos, and
 * only when the built-in is enabled). This module is the ONE writer of the
 * map, and it writes with the two rows locked, so:
 *
 *   - a failure between the writes rolls back (no enrollment row the map
 *     lacks — a repo nothing reviews);
 *   - two concurrent enrollments for different repos serialize on the
 *     automation row lock, and the map is patched with a JSONB expression
 *     in place, so neither clobbers the other's entry (no lost update).
 *
 * Phase 4.7b folds `review_enrollment` into the `repos` input and deletes
 * this module; the map becomes the single source.
 */

import { and, eq, isNull, sql } from "drizzle-orm";

import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";
import { getDb } from "./client.ts";
import { automation as automationTable, reviewEnrollment as enrollmentTable } from "./schema.ts";
import type { EnrollmentRow } from "./enrollments.ts";

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

export type SyncEnrollmentResult =
  /** The repo is not enrolled; nothing was written. */
  | { kind: "not_enrolled" }
  /** The built-in is not seeded; nothing was written. */
  | { kind: "not_seeded" }
  | { kind: "applied"; enrollment: EnrollmentRow };

export interface ReviewEnrollmentSync {
  /** Mirror the enrollment row into the built-in's `repos` map and enable
   * the built-in (the first repo turns reviewing on). */
  enrolled(repo: string): Promise<SyncEnrollmentResult>;
  /** Take the repo out of the map. The enrollment row is the caller's. */
  removed(repo: string): Promise<{ kind: "not_seeded" | "applied" }>;
}

function toEnrollmentRow(row: typeof enrollmentTable.$inferSelect): EnrollmentRow {
  return {
    repo: row.repo,
    triggerMode: row.triggerMode as EnrollmentRow["triggerMode"],
    autofix: row.autofix as EnrollmentRow["autofix"],
    profileId: row.profileId ?? null,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

export function makeReviewEnrollmentSync(
  db: ReturnType<typeof getDb> = getDb(),
): ReviewEnrollmentSync {
  return {
    async enrolled(repo) {
      return db.transaction(async (tx) => {
        // Lock the enrollment row, and the automation row so concurrent
        // enrollments for other repos serialize behind this one.
        const [enrollment] = await tx
          .select()
          .from(enrollmentTable)
          .where(eq(enrollmentTable.repo, repo))
          .for("update");
        if (!enrollment) return { kind: "not_enrolled" as const };

        const [builtin] = await tx
          .select({ id: automationTable.id, enabled: automationTable.enabled })
          .from(automationTable)
          .where(and(eq(automationTable.builtinKey, PR_REVIEW_BUILTIN_KEY), isNull(automationTable.archivedAt)))
          .for("update");
        if (!builtin) return { kind: "not_seeded" as const };

        // `inputs.repos.<repo> = policy`, in place. The `||` seeds a missing
        // `repos` object first: jsonb_set creates only the LAST path element.
        const policy = JSON.stringify(repoPolicy(toEnrollmentRow(enrollment)));
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

        return { kind: "applied" as const, enrollment: toEnrollmentRow(enrollment) };
      });
    },

    async removed(repo) {
      return db.transaction(async (tx) => {
        const [builtin] = await tx
          .select({ id: automationTable.id })
          .from(automationTable)
          .where(and(eq(automationTable.builtinKey, PR_REVIEW_BUILTIN_KEY), isNull(automationTable.archivedAt)))
          .for("update");
        if (!builtin) return { kind: "not_seeded" as const };
        await tx
          .update(automationTable)
          .set({
            inputs: sql`${automationTable.inputs} #- ARRAY['repos', ${repo}::text]`,
            updatedAt: new Date(),
          })
          .where(eq(automationTable.id, builtin.id));
        return { kind: "applied" as const };
      });
    },
  };
}
