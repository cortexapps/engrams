/** Live-PG proof of the review engine flip's two guarantees: the flag and
 * the built-in's repos map move together (one transaction), and concurrent
 * flips for different repos never lose each other's map entry. Runs when
 * ORCHESTRATOR_DATABASE_URL points at a migrated database (CI's
 * orchestrator lane), and skips otherwise. */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { eq } from "drizzle-orm";
import { Pool } from "pg";

import * as schema from "../db/schema.ts";
import { makeReviewEngineWriter } from "../db/review-engine.ts";
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
  await db
    .insert(schema.reviewEnrollment)
    .values(REPOS.map((repo) => ({ repo, triggerMode: "auto", autofix: "off", engine: "legacy" })));
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

describe("review engine writer (live PG)", () => {
  test.skipIf(!reachable)(
    "flips the row and patches the map together; a legacy flip removes the entry",
    async () => {
      const writer = makeReviewEngineWriter(db!);
      const result = await writer.apply("rw-test/alpha", "automation");
      expect(result.kind).toBe("applied");
      if (result.kind !== "applied") return;
      expect(result.enrollment.engine).toBe("automation");
      expect(result.builtinEnabled).toBe(true);
      expect((await reposMap())["rw-test/alpha"]).toEqual({ mode: "auto", autofix: false });

      const back = await writer.apply("rw-test/alpha", "legacy");
      expect(back.kind).toBe("applied");
      expect((await reposMap())["rw-test/alpha"]).toBeUndefined();
      const [row] = await db!
        .select()
        .from(schema.reviewEnrollment)
        .where(eq(schema.reviewEnrollment.repo, "rw-test/alpha"));
      expect(row?.engine).toBe("legacy");
    },
  );

  test.skipIf(!reachable)(
    "concurrent flips for different repos all land in the map (no lost update)",
    async () => {
      const writer = makeReviewEngineWriter(db!);
      await Promise.all(
        ["rw-test/beta", "rw-test/gamma", "rw-test/delta"].map((r) => writer.apply(r, "automation")),
      );
      const map = await reposMap();
      expect(map["rw-test/beta"]).toEqual({ mode: "auto", autofix: false });
      expect(map["rw-test/gamma"]).toEqual({ mode: "auto", autofix: false });
      expect(map["rw-test/delta"]).toEqual({ mode: "auto", autofix: false });
    },
  );

  test.skipIf(!reachable)("an unenrolled repo writes nothing", async () => {
    const writer = makeReviewEngineWriter(db!);
    expect(await writer.apply("rw-test/nobody", "automation")).toEqual({ kind: "not_enrolled" });
  });
});
