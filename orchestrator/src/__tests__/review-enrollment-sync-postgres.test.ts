/** Live-PG proof of the enrollment sync's guarantees: the enrollment row
 * and the built-in's repos map move together (one transaction, and no row
 * without a seeded built-in), and concurrent enrollments for different
 * repos never lose each other's map entry. Runs when ORCHESTRATOR_DATABASE_URL points at a migrated database
 * (CI's orchestrator lane), and skips otherwise. */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { eq } from "drizzle-orm";
import { Pool } from "pg";

import * as schema from "../db/schema.ts";
import { makeReviewEnrollmentSync } from "../db/review-enrollment-sync.ts";
import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 8 }) : null;
let reachable = false;
if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}
const db = pool ? drizzle(pool, { schema }) : null;

const REPOS = ["rw-test/alpha", "rw-test/beta", "rw-test/gamma", "rw-test/delta"] as const;
let builtinId = "";
let hadBuiltin = false;

beforeAll(async () => {
  if (!reachable || !db) return;
  // Use the seeded built-in when the database has one (a dev DB), else
  // plant a minimal row under the built-in key and remove it afterwards.
  const [existing] = await db
    .select({ id: schema.automation.id })
    .from(schema.automation)
    .where(eq(schema.automation.builtinKey, PR_REVIEW_BUILTIN_KEY));
  if (existing) {
    builtinId = existing.id;
    hadBuiltin = true;
  } else {
    builtinId = randomUUID();
    await db.insert(schema.automation).values({
      id: builtinId,
      name: "PR review (test)",
      description: "",
      enabled: false,
      kind: "builtin",
      builtinKey: PR_REVIEW_BUILTIN_KEY,
      currentVersion: 1,
      inputs: {},
      endSessionsOnFinish: true,
      nextFireAt: null,
      createdByUserId: null,
    });
  }
});

afterAll(async () => {
  if (!reachable || !db) return;
  for (const repo of REPOS) {
    await db.delete(schema.reviewEnrollment).where(eq(schema.reviewEnrollment.repo, repo));
  }
  if (hadBuiltin) {
    // Strip only our entries from the real built-in's map.
    const [row] = await db
      .select({ inputs: schema.automation.inputs })
      .from(schema.automation)
      .where(eq(schema.automation.id, builtinId));
    const repos = { ...((row?.inputs["repos"] as Record<string, unknown> | undefined) ?? {}) };
    for (const repo of REPOS) delete repos[repo];
    await db
      .update(schema.automation)
      .set({ inputs: { ...row!.inputs, repos } })
      .where(eq(schema.automation.id, builtinId));
  } else {
    await db.delete(schema.automation).where(eq(schema.automation.id, builtinId));
  }
  await pool?.end();
});

async function reposMap(): Promise<Record<string, unknown>> {
  const [row] = await db!
    .select({ inputs: schema.automation.inputs })
    .from(schema.automation)
    .where(eq(schema.automation.id, builtinId));
  return (row?.inputs["repos"] as Record<string, unknown> | undefined) ?? {};
}

describe("review enrollment sync (live PG)", () => {
  const enrollment = (repo: string) => ({ repo, triggerMode: "auto" as const, autofix: "off" as const, profileId: null });
  const rowFor = async (repo: string) =>
    (await db!.select().from(schema.reviewEnrollment).where(eq(schema.reviewEnrollment.repo, repo)))[0] ?? null;

  test.skipIf(!reachable)(
    "writes the row and the map together and enables the built-in; unenroll removes both",
    async () => {
      const sync = makeReviewEnrollmentSync(db!);
      const result = await sync.enroll(enrollment("rw-test/alpha"));
      expect(result.kind).toBe("applied");
      if (result.kind !== "applied") return;
      expect(result.enrollment.repo).toBe("rw-test/alpha");
      expect((await rowFor("rw-test/alpha"))?.triggerMode).toBe("auto");
      expect((await reposMap())["rw-test/alpha"]).toEqual({ mode: "auto", autofix: false });
      const [builtin] = await db!
        .select({ enabled: schema.automation.enabled })
        .from(schema.automation)
        .where(eq(schema.automation.id, builtinId));
      expect(builtin?.enabled).toBe(true);

      // A second enroll is an update of the same row (no duplicate key).
      const again = await sync.enroll({ ...enrollment("rw-test/alpha"), triggerMode: "manual" });
      expect(again.kind).toBe("applied");
      expect((await reposMap())["rw-test/alpha"]).toEqual({ mode: "on_request", autofix: false });

      await sync.unenroll("rw-test/alpha");
      expect(await rowFor("rw-test/alpha")).toBeNull();
      expect((await reposMap())["rw-test/alpha"]).toBeUndefined();
    },
  );

  test.skipIf(!reachable)(
    "concurrent enrollments for different repos all land in the map (no lost update)",
    async () => {
      const sync = makeReviewEnrollmentSync(db!);
      await Promise.all(
        ["rw-test/beta", "rw-test/gamma", "rw-test/delta"].map((r) => sync.enroll(enrollment(r))),
      );
      const map = await reposMap();
      expect(map["rw-test/beta"]).toEqual({ mode: "auto", autofix: false });
      expect(map["rw-test/gamma"]).toEqual({ mode: "auto", autofix: false });
      expect(map["rw-test/delta"]).toEqual({ mode: "auto", autofix: false });
    },
  );

  test.skipIf(!reachable)("unenroll of a repo that was never enrolled is a no-op", async () => {
    const sync = makeReviewEnrollmentSync(db!);
    await expect(sync.unenroll("rw-test/nobody")).resolves.toBeUndefined();
  });
});
