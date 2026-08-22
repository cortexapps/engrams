/** Pull-request review enrollment data-access seam (ADR 0100). */

import { asc, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { reviewEnrollment as enrollmentTable } from "./schema.ts";

export type ReviewTriggerMode = "auto" | "manual";
export type ReviewAutofix = "auto" | "manual" | "off";
/** ADR 0119 phase 4.4: the review engine for a repo during the parallel window. */
export type ReviewEngine = "legacy" | "automation";

export interface EnrollmentInput {
  repo: string;
  triggerMode: ReviewTriggerMode;
  autofix: ReviewAutofix;
  profileId: string | null;
  /** Defaults to "legacy" when omitted (an upsert never silently flips it). */
  engine?: ReviewEngine;
}

export interface EnrollmentRow extends Omit<EnrollmentInput, "engine"> {
  engine: ReviewEngine;
  createdAt: Date;
  updatedAt: Date;
}

export interface EnrollmentStore {
  list(): Promise<EnrollmentRow[]>;
  get(repo: string): Promise<EnrollmentRow | null>;
  upsert(input: EnrollmentInput): Promise<EnrollmentRow>;
  /** Flip the engine only; null when the repo is not enrolled. */
  setEngine(repo: string, engine: ReviewEngine): Promise<EnrollmentRow | null>;
  delete(repo: string): Promise<void>;
}

function toRow(row: typeof enrollmentTable.$inferSelect): EnrollmentRow {
  return {
    repo: row.repo,
    triggerMode: row.triggerMode as ReviewTriggerMode,
    autofix: row.autofix as ReviewAutofix,
    profileId: row.profileId ?? null,
    engine: row.engine === "automation" ? "automation" : "legacy",
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
      const { engine, ...rest } = input;
      const rows = await db
        .insert(enrollmentTable)
        .values({ ...rest, ...(engine !== undefined ? { engine } : {}) })
        .onConflictDoUpdate({
          target: enrollmentTable.repo,
          set: {
            triggerMode: input.triggerMode,
            autofix: input.autofix,
            profileId: input.profileId,
            // Omitted engine keeps the stored value: a profile/trigger edit
            // must never silently move a repo between engines.
            ...(engine !== undefined ? { engine } : {}),
            updatedAt: new Date(),
          },
        })
        .returning();
      const row = rows[0];
      if (!row) throw new Error("review enrollment upsert returned no row");
      return toRow(row);
    },

    async setEngine(repo, engine) {
      const rows = await db
        .update(enrollmentTable)
        .set({ engine, updatedAt: new Date() })
        .where(eq(enrollmentTable.repo, repo))
        .returning();
      return rows[0] ? toRow(rows[0]) : null;
    },

    async delete(repo) {
      await db.delete(enrollmentTable).where(eq(enrollmentTable.repo, repo));
    },
  };
}
