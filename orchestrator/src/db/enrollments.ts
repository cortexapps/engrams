/** Pull-request review enrollment reads (ADR 0100). The writes live in
 * review-enrollment-sync.ts, which keeps the row and the PR-review
 * built-in's `repos` map in one transaction. */

import { asc, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { reviewEnrollment as enrollmentTable } from "./schema.ts";

export type ReviewTriggerMode = "auto" | "manual";
export type ReviewAutofix = "auto" | "manual" | "off";

export interface EnrollmentInput {
  repo: string;
  triggerMode: ReviewTriggerMode;
  autofix: ReviewAutofix;
  profileId: string | null;
}

export interface EnrollmentRow extends EnrollmentInput {
  createdAt: Date;
  updatedAt: Date;
}

export interface EnrollmentStore {
  list(): Promise<EnrollmentRow[]>;
  get(repo: string): Promise<EnrollmentRow | null>;
}

function toRow(row: typeof enrollmentTable.$inferSelect): EnrollmentRow {
  return {
    repo: row.repo,
    triggerMode: row.triggerMode as ReviewTriggerMode,
    autofix: row.autofix as ReviewAutofix,
    profileId: row.profileId ?? null,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

export function makeEnrollmentStore(
  db: ReturnType<typeof getDb> = getDb(),
): EnrollmentStore {
  return {
    async list() {
      const rows = await db
        .select()
        .from(enrollmentTable)
        .orderBy(asc(enrollmentTable.repo));
      return rows.map(toRow);
    },

    async get(repo) {
      const rows = await db
        .select()
        .from(enrollmentTable)
        .where(eq(enrollmentTable.repo, repo))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

  };
}
