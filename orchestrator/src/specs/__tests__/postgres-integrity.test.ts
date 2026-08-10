import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";

import { PostgresOpenQuestionStore } from "../open-questions.ts";
import { PostgresSectionStateStore } from "../section-state-service.ts";
import { transitionSectionState } from "../section-state.ts";
import { PostgresSpecToolMetadataStore } from "../tool-service.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 6 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

describe("spec integrity stores with live Postgres", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const lifecycleSpecId = randomUUID();
  const actorUserId = `spec-tool-actor-${randomUUID()}`;
  const samUserId = `spec-tool-sam-${randomUUID()}`;
  const duplicateSamUserId = `spec-tool-sam-duplicate-${randomUUID()}`;
  const expiredUserId = `spec-tool-expired-${randomUUID()}`;
  const disconnectedUserId = `spec-tool-disconnected-${randomUUID()}`;
  const userIds = [actorUserId, samUserId, duplicateSamUserId, expiredUserId, disconnectedUserId];

  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO spec_template
         (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Integrity test template', '[]', '[]', '{}')`,
      [templateId],
    );
    await pool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, 'Integrity test spec', 'draft'),
              ($3, 'test-org', $2, 'Lifecycle test spec', 'draft')`,
      [specId, templateId, lifecycleSpecId],
    );
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES
         ($1, 'Ari', $1 || '@example.test', false, now(), now()),
         ($2, 'Sam', $2 || '@example.test', false, now(), now()),
         ($3, 'Sam', $3 || '@example.test', false, now(), now()),
         ($4, 'Crashed editor', $4 || '@example.test', false, now(), now()),
         ($5, 'Old editor', $5 || '@example.test', false, now(), now())`,
      userIds,
    );
    const connectedAt = new Date("2026-08-09T12:00:00.000Z");
    const activeLeaseExpiresAt = new Date("2026-08-09T12:01:00.000Z");
    const expiredLeaseExpiresAt = new Date("2026-08-09T12:00:10.000Z");
    const disconnectedAt = new Date("2026-08-09T12:00:30.000Z");
    await pool.query(
      `INSERT INTO spec_participant
         (spec_id, client_id, user_id, connection_epoch, connected_at,
          disconnected_at, lease_expires_at)
       VALUES
         ($1, 'actor', $2, 1, $6, NULL, $7),
         ($1, 'sam-1', $3, 1, $6, NULL, $7),
         ($1, 'sam-2', $4, 1, $6, NULL, $7),
         ($1, 'crashed', $5, 1, $6, NULL, $8),
         ($1, 'old', $9, 1, $6, $10, $7)`,
      [
        specId,
        actorUserId,
        samUserId,
        duplicateSamUserId,
        expiredUserId,
        connectedAt,
        activeLeaseExpiresAt,
        expiredLeaseExpiresAt,
        disconnectedUserId,
        disconnectedAt,
      ],
    );
  });

  afterAll(async () => {
    if (!pool) return;
    if (reachable) {
      await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [
        [specId, lifecycleSpecId],
      ]);
      await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
      await pool.query("DELETE FROM \"user\" WHERE id = ANY($1::text[])", [userIds]);
    }
    await pool.end();
    pool = null;
  });

  test.skipIf(!reachable)("only one concurrent section state action wins", async () => {
    if (!pool) throw new Error("The live Postgres pool is not available.");
    const store = new PostgresSectionStateStore(pool);
    const expected = { state: "empty" as const, naReason: null };
    const sectionContext = {
      specId,
      sectionId: "requirements",
      sectionTitle: "Requirements",
      allowsNa: true,
    };
    const drafted = transitionSectionState(expected, "drafted", sectionContext);
    const notApplicable = transitionSectionState(
      expected,
      "n/a",
      sectionContext,
      "No requirement applies.",
    );

    const inputs = [
      {
        actionId: randomUUID(),
        specId,
        sectionId: "requirements",
        requestFingerprint: "drafted-command",
        expected,
        next: drafted.value,
        confirmedBy: null,
        chip: drafted.transcriptChip,
        at: new Date("2026-08-09T12:00:00.000Z"),
      },
      {
        actionId: randomUUID(),
        specId,
        sectionId: "requirements",
        requestFingerprint: "not-applicable-command",
        expected,
        next: notApplicable.value,
        confirmedBy: null,
        chip: notApplicable.transcriptChip,
        at: new Date("2026-08-09T12:00:01.000Z"),
      },
    ];
    const results = await Promise.all(inputs.map((input) => store.persistStateAction(input)));

    expect(results.filter(({ status }) => status === "stored")).toHaveLength(1);
    expect(results.filter(({ status }) => status === "conflict")).toHaveLength(1);
    expect((await store.read(specId, "requirements")).state).not.toBe("empty");
    const winnerIndex = results.findIndex(({ status }) => status === "stored");
    const replay = await store.persistStateAction(inputs[winnerIndex]!);
    expect(replay.status).toBe("replayed");
  });

  test.skipIf(!reachable)(
    "a state action checks lifecycle and updates list time in its transaction",
    async () => {
      if (!pool) throw new Error("The live Postgres pool is not available.");
      const store = new PostgresSectionStateStore(pool);
      const context = {
        specId: lifecycleSpecId,
        sectionId: "context",
        sectionTitle: "Context",
        allowsNa: false,
      };
      const expected = { state: "empty" as const, naReason: null };
      const drafted = transitionSectionState(expected, "drafted", context);
      const actionAt = new Date("2026-08-10T12:00:00.000Z");
      const input = {
        actionId: randomUUID(),
        specId: lifecycleSpecId,
        sectionId: context.sectionId,
        requestFingerprint: "draft-context",
        expected,
        next: drafted.value,
        confirmedBy: null,
        chip: drafted.transcriptChip,
        at: actionAt,
      };

      expect((await store.persistStateAction(input)).status).toBe("stored");
      const afterAction = await pool.query<{ updated_at: Date }>(
        "SELECT updated_at FROM spec WHERE id = $1",
        [lifecycleSpecId],
      );
      expect(afterAction.rows[0]?.updated_at).toEqual(actionAt);

      const publishedAt = new Date("2026-08-10T12:01:00.000Z");
      await pool.query("UPDATE spec SET lifecycle = 'published', updated_at = $2 WHERE id = $1", [
        lifecycleSpecId,
        publishedAt,
      ]);
      expect((await store.persistStateAction(input)).status).toBe("replayed");

      const otherContext = { ...context, sectionId: "design", sectionTitle: "Design" };
      const otherDrafted = transitionSectionState(expected, "drafted", otherContext);
      const rejected = await store.persistStateAction({
        ...input,
        actionId: randomUUID(),
        sectionId: otherContext.sectionId,
        requestFingerprint: "draft-design",
        chip: otherDrafted.transcriptChip,
      });
      expect(rejected.status).toBe("read_only");
      const durable = await pool.query<{
        updated_at: Date;
        states: string;
        actions: string;
      }>(
        `SELECT s.updated_at,
                (SELECT count(*)::text FROM spec_section_state st WHERE st.spec_id = s.id) AS states,
                (SELECT count(*)::text FROM spec_transcript_action a WHERE a.spec_id = s.id) AS actions
           FROM spec s
          WHERE s.id = $1`,
        [lifecycleSpecId],
      );
      expect(durable.rows[0]).toEqual({ updated_at: publishedAt, states: "1", actions: "1" });
    },
  );

  test.skipIf(!reachable)("only one concurrent question row resolution wins", async () => {
    if (!pool) throw new Error("The live Postgres pool is not available.");
    const store = new PostgresOpenQuestionStore(pool);
    const id = randomUUID();
    await store.create({
      id,
      specId,
      sectionId: "failure-modes",
      text: "How many retries?",
      openedBy: null,
      requestFingerprint: "question-command",
    });

    const results = await Promise.all([
      store.resolve({
        id,
        expectedState: "open",
        resolutionLink: "yjs-section://failure-modes/first",
        resolvedAt: new Date("2026-08-09T12:00:00.000Z"),
      }),
      store.resolve({
        id,
        expectedState: "open",
        resolutionLink: "yjs-section://failure-modes/second",
        resolvedAt: new Date("2026-08-09T12:00:01.000Z"),
      }),
    ]);

    expect(results.filter(Boolean)).toHaveLength(1);
    expect((await store.find(id))?.state).toBe("resolved");
  });

  test.skipIf(!reachable)("lists only other current editor names", async () => {
    if (!pool) throw new Error("The live Postgres pool is not available.");
    const store = new PostgresSpecToolMetadataStore(
      pool,
      () => new Date("2026-08-09T12:00:20.000Z"),
    );

    expect(await store.concurrentEditorNames(specId, actorUserId)).toEqual(["Sam"]);
  });
});
