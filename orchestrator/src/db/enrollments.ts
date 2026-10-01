/** Pull-request review enrollment data-access seam (ADR 0100). */

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
  upsert(input: EnrollmentInput): Promise<EnrollmentRow>;
  delete(repo: string): Promise<void>;
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

    async upsert(input) {
      const rows = await db
        .insert(enrollmentTable)
        .values(input)
        .onConflictDoUpdate({
          target: enrollmentTable.repo,
          set: {
            triggerMode: input.triggerMode,
            autofix: input.autofix,
            profileId: input.profileId,
            updatedAt: new Date(),
          },
        })
        .returning();
      const row = rows[0];
      if (!row) throw new Error("review enrollment upsert returned no row");
      return toRow(row);
    },

    async delete(repo) {
      await db.delete(enrollmentTable).where(eq(enrollmentTable.repo, repo));
    },
  };
}
