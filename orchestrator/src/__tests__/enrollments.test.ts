import { describe, expect, test } from "bun:test";
import { eq } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { makeEnrollmentStore } from "../db/enrollments.ts";
import { reviewEnrollment as enrollmentTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("EnrollmentStore", () => {
  test.skipIf(!dbReachable)("CRUD and conflict update", async () => {
    const db = getDb();
    const store = makeEnrollmentStore(db);
    const repo = `enrollment-${crypto.randomUUID()}/engrams`;

    try {
      const created = await store.upsert({
        repo,
        triggerMode: "manual",
        autofix: "off",
        profileId: null,
      });
      expect(created).toMatchObject({
        repo,
        triggerMode: "manual",
        autofix: "off",
        profileId: null,
      });
      expect((await store.get(repo))?.repo).toBe(repo);
      expect((await store.list()).some((row) => row.repo === repo)).toBe(true);

      const updated = await store.upsert({
        repo,
        triggerMode: "auto",
        autofix: "manual",
        profileId: null,
      });
      expect(updated).toMatchObject({
        repo,
        triggerMode: "auto",
        autofix: "manual",
      });

      await store.delete(repo);
      expect(await store.get(repo)).toBeNull();
    } finally {
      await db.delete(enrollmentTable).where(eq(enrollmentTable.repo, repo));
    }
  });
});
