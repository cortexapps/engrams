import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import * as schema from "../db/schema.ts";
import {
  ENGINEERING_DESIGN_TEMPLATE,
  SpecTemplateCatalog,
  makePostgresSpecTemplateStore,
} from "../specs/template-catalog.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool = DB_URL ? new Pool({ connectionString: DB_URL }) : null;
const reachable = pool
  ? await pool
      .query("SELECT 1")
      .then(() => true)
      .catch(() => false)
  : false;

describe.skipIf(!reachable)("spec template store", () => {
  const orgId = `spec-template-test-${randomUUID()}`;
  const templateId = randomUUID();
  let catalog: SpecTemplateCatalog;

  beforeAll(() => {
    if (!pool) throw new Error("Postgres is not available");
    catalog = new SpecTemplateCatalog(
      makePostgresSpecTemplateStore(drizzle(pool, { schema })),
      () => templateId,
      () => new Date("2026-08-11T01:00:00.000Z"),
    );
  });

  afterAll(async () => {
    if (!pool) return;
    await pool.query("DELETE FROM spec_template WHERE org_id = $1", [orgId]);
    await pool.end();
  });

  test("persists an organization template and keeps it outside other organizations", async () => {
    const builtIn = (await catalog.list(orgId)).find(
      (item) => item.id === "00000000-0000-4000-8000-000000000115",
    );
    expect(builtIn).toMatchObject({ builtIn: true, modifiedFromDefault: false });

    const created = await catalog.create(orgId, {
      ...structuredClone(ENGINEERING_DESIGN_TEMPLATE),
      name: "Service design",
    });

    expect(created).toMatchObject({ id: templateId, name: "Service design", builtIn: false });
    expect((await catalog.list(orgId)).some((item) => item.id === templateId)).toBe(true);
    expect((await catalog.list("another-org")).some((item) => item.id === templateId)).toBe(false);
  });
});
