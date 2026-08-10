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
  const actorUserId = `spec-tool-actor-${randomUUID()}`;
  const samUserId = `spec-tool-sam-${randomUUID()}`;
  const duplicateSamUserId = `spec-tool-sam-duplicate-${randomUUID()}`;
  const disconnectedUserId = `spec-tool-disconnected-${randomUUID()}`;
  const userIds = [actorUserId, samUserId, duplicateSamUserId, disconnectedUserId];

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
       VALUES ($1, 'test-org', $2, 'Integrity test spec', 'draft')`,
      [specId, templateId],
    );
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES
         ($1, 'Ari', $1 || '@example.test', false, now(), now()),
         ($2, 'Sam', $2 || '@example.test', false, now(), now()),
         ($3, 'Sam', $3 || '@example.test', false, now(), now()),
         ($4, 'Old editor', $4 || '@example.test', false, now(), now())`,
      userIds,
    );
    await pool.query(
      `INSERT INTO spec_participant
         (spec_id, client_id, user_id, connected_at, disconnected_at)
       VALUES
         ($1, 'actor', $2, now(), NULL),
         ($1, 'sam-1', $3, now(), NULL),
         ($1, 'sam-2', $4, now(), NULL),
         ($1, 'old', $5, now(), now())`,
      [specId, ...userIds],
    );
  });

  afterAll(async () => {
    if (!pool) return;
    if (reachable) {
      await pool.query("DELETE FROM spec WHERE id = $1", [specId]);
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

  test.skipIf(!reachable)("lists only other active editor names", async () => {
    if (!pool) throw new Error("The live Postgres pool is not available.");
    const store = new PostgresSpecToolMetadataStore(pool);

    expect(await store.concurrentEditorNames(specId, actorUserId)).toEqual(["Sam"]);
  });
});
