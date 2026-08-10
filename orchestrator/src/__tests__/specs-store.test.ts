import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import * as schema from "../db/schema.ts";
import { makeSpecListStore } from "../db/specs.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool = DB_URL ? new Pool({ connectionString: DB_URL }) : null;
const reachable = pool
  ? await pool
      .query("SELECT 1")
      .then(() => true)
      .catch(() => false)
  : false;

describe.skipIf(!reachable)("spec list store", () => {
  const now = new Date("2026-08-10T12:00:00Z");
  const ids = {
    spec: randomUUID(),
    otherSpec: randomUUID(),
    template: randomUUID(),
    session: randomUUID(),
    task: `spec-list-${randomUUID()}`,
    activeUser: `spec-list-active-${randomUUID()}`,
    expiredUser: `spec-list-expired-${randomUUID()}`,
  };

  beforeAll(async () => {
    if (!pool) throw new Error("Postgres is not available");
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Active Person', $2, false, $3, $3),
              ($4, 'Expired Person', $5, false, $3, $3)`,
      [ids.activeUser, `${ids.activeUser}@test`, now, ids.expiredUser, `${ids.expiredUser}@test`],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Design', '[]', '[]', '{}')`,
      [ids.template],
    );
    await pool.query(`INSERT INTO task (id, type, launch_policy) VALUES ($1, 'chat', $2)`, [
      ids.task,
      { repos: [{ path: "/workspace/engrams" }] },
    ]);
    await pool.query(`INSERT INTO task_session (task_id, session_id) VALUES ($1, $2)`, [
      ids.task,
      ids.session,
    ]);
    await pool.query(
      `INSERT INTO spec (id, org_id, session_id, template_id, title, lifecycle, updated_at)
       VALUES ($1, 'org-a', $2, $3, 'Visible spec', 'draft', $4),
              ($5, 'org-b', NULL, $3, 'Other organization', 'published', $4)`,
      [ids.spec, ids.session, ids.template, now, ids.otherSpec],
    );
    await pool.query(
      `INSERT INTO spec_participant
         (spec_id, client_id, user_id, connection_epoch, connected_at, lease_expires_at)
       VALUES ($1, 'active', $2, 1, $3, $4), ($1, 'expired', $5, 1, $3, $6)`,
      [
        ids.spec,
        ids.activeUser,
        now,
        new Date(now.getTime() + 60_000),
        ids.expiredUser,
        new Date(now.getTime() - 1),
      ],
    );
    await pool.query(
      `INSERT INTO spec_open_question
         (id, spec_id, section_id, text, request_fingerprint, state)
       VALUES ($1, $2, 'scope', 'Open?', 'open', 'open'),
              ($3, $2, 'scope', 'Closed?', 'closed', 'resolved')`,
      [randomUUID(), ids.spec, randomUUID()],
    );
  });

  afterAll(async () => {
    if (!pool) return;
    await pool.query("DELETE FROM spec WHERE id = ANY($1)", [[ids.spec, ids.otherSpec]]);
    await pool.query("DELETE FROM task WHERE id = $1", [ids.task]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [ids.template]);
    await pool.query('DELETE FROM "user" WHERE id = ANY($1)', [[ids.activeUser, ids.expiredUser]]);
    await pool.end();
  });

  test("isolates organizations and excludes a pod-abandoned expired participant", async () => {
    if (!pool) throw new Error("Postgres is not available");
    const store = makeSpecListStore(drizzle(pool, { schema }), () => now);
    const result = await store.list({ orgId: "org-a", page: 1, pageSize: 50 });

    expect(result.totalCount).toBe(1);
    expect(result.rows).toHaveLength(1);
    expect(result.rows[0]).toMatchObject({
      title: "Visible spec",
      repo: "engrams",
      openQuestionCount: 1,
      participants: [{ id: ids.activeUser, name: "Active Person" }],
    });
  });
});
