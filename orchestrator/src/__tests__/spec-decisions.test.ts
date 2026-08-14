import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";

import {
  makeSpecDecisionsRoute,
  PostgresSpecDecisionStore,
  type SpecDecisionStore,
} from "../routes/spec-decisions.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 2 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

const SPEC_ID = randomUUID();
const TEMPLATE_ID = randomUUID();
const CHECKPOINT_ID = randomUUID();
const OWNER_ID = `decisions-owner-${randomUUID()}`;
const MEMBER_ID = `decisions-member-${randomUUID()}`;
const SETTLE_ACTOR_ID = `decisions-settler-${randomUUID()}`;
const DELETED_ACTOR_ID = `decisions-deleted-${randomUUID()}`;
const SETTLE_ID = `settle-${randomUUID()}`;
const LATE_SETTLE_ID = `late-settle-${randomUUID()}`;
const QUESTION_ID = randomUUID();
const SETTLED_AT = new Date("2026-08-13T18:00:00.000Z");
const RESOLVED_AT = new Date("2026-08-13T18:01:00.000Z");
const PUBLISHED_AT = new Date("2026-08-13T18:02:00.000Z");
const LATE_SETTLED_AT = new Date("2026-08-13T18:03:00.000Z");

function memberRoute(store: SpecDecisionStore, userId = MEMBER_ID, member = true) {
  return makeSpecDecisionsRoute({
    store,
    resolveMembership: async (specId, candidate) =>
      member && specId === SPEC_ID && candidate === userId,
    getSession: async () => ({ user: { id: userId } }),
  });
}

describe("spec decisions route", () => {
  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Spec owner', $1 || '@example.test', false, now(), now()),
              ($2, 'Organization member', $2 || '@example.test', false, now(), now()),
              ($3, 'Priya Raman', $3 || '@example.test', false, now(), now()),
              ($4, 'Former member', $4 || '@example.test', false, now(), now())`,
      [OWNER_ID, MEMBER_ID, SETTLE_ACTOR_ID, DELETED_ACTOR_ID],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections)
       VALUES ($1, 'Decision test template', '[]', $2)`,
      [
        TEMPLATE_ID,
        JSON.stringify([
          {
            key: "data",
            title: "Data model",
            layerKey: "contract",
            guidance: "Define the data model.",
            doneCriteria: [],
            required: true,
            allowNa: false,
          },
        ]),
      ],
    );
    await pool.query(
      `INSERT INTO spec
         (id, org_id, owner_user_id, template_id, title, phase, current_doc_seq,
          current_semantic_doc_seq)
       VALUES ($1, 'test-org', $2, $3, 'Decision record', 'drafting', 8, 8)`,
      [SPEC_ID, OWNER_ID, TEMPLATE_ID],
    );
    await pool.query(
      `INSERT INTO spec_transcript_action
         (id, spec_id, section_id, request_fingerprint, chip, result, actor_user_id,
          created_at, delivered_at)
       VALUES ($1, $2, 'data', 'settle-fingerprint', $3, '{}', $4, $5, $5),
              ($6, $2, 'data', 'late-settle-fingerprint', $7, '{}', $4, $8, $8)`,
      [
        SETTLE_ID,
        SPEC_ID,
        JSON.stringify({
          kind: "spec_section_state_changed",
          specId: SPEC_ID,
          sectionId: "data",
          sectionTitle: "Data model",
          before: { state: "proposed", naReason: null },
          after: { state: "settled", naReason: null },
          undo: { state: "proposed", naReason: null },
        }),
        SETTLE_ACTOR_ID,
        SETTLED_AT,
        LATE_SETTLE_ID,
        JSON.stringify({
          kind: "spec_section_state_changed",
          specId: SPEC_ID,
          sectionId: "data",
          sectionTitle: "A later title",
          before: { state: "proposed", naReason: null },
          after: { state: "settled", naReason: null },
          undo: { state: "proposed", naReason: null },
        }),
        LATE_SETTLED_AT,
      ],
    );
    await pool.query(
      `INSERT INTO spec_open_question
         (id, spec_id, section_id, text, request_fingerprint, state,
          resolution_note, resolved_by, resolved_at, created_at)
       VALUES ($1, $2, 'data', 'Which lock coordinates writers?', 'question-fingerprint',
               'resolved', 'section:data@resolution', $3, $4, $5)`,
      [QUESTION_ID, SPEC_ID, DELETED_ACTOR_ID, RESOLVED_AT, SETTLED_AT],
    );
    await pool.query(
      `INSERT INTO spec_checkpoint
         (id, spec_id, state, state_vector, rendered_markdown, doc_seq, label, reason,
          created_at)
       VALUES ($1, $2, $3, $4, '# Decision record', 8, 'Published version', 'publish', $5)`,
      [CHECKPOINT_ID, SPEC_ID, Buffer.from([1]), Buffer.from([2]), PUBLISHED_AT],
    );
    await pool.query(
      `UPDATE spec
          SET phase = 'published', published_checkpoint_id = $2, published_by = $3,
              published_at = $4
        WHERE id = $1`,
      [SPEC_ID, CHECKPOINT_ID, OWNER_ID, PUBLISHED_AT],
    );
    // Both actor foreign keys use ON DELETE SET NULL. The decision survives,
    // and the read model supplies a stable display label for the deleted user.
    await pool.query('DELETE FROM "user" WHERE id = $1', [DELETED_ACTOR_ID]);
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = $1", [SPEC_ID]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [TEMPLATE_ID]);
    await pool.query('DELETE FROM "user" WHERE id = ANY($1::text[])', [
      [OWNER_ID, MEMBER_ID, SETTLE_ACTOR_ID],
    ]);
    await pool.end();
  });

  test.skipIf(!reachable)(
    "is member-gated, ordered, includes both decision kinds, and tolerates a deleted actor",
    async () => {
      const store = new PostgresSpecDecisionStore(pool!);
      const response = await memberRoute(store).request(`/api/v1/specs/${SPEC_ID}/decisions`);

      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({
        decisions: [
          {
            id: SETTLE_ID,
            kind: "section_settled",
            sectionId: "data",
            sectionTitle: "Data model",
            actor: { id: SETTLE_ACTOR_ID, name: "Priya Raman" },
            decidedAt: SETTLED_AT.toISOString(),
          },
          {
            id: QUESTION_ID,
            kind: "question_resolved",
            sectionId: "data",
            sectionTitle: "Data model",
            question: "Which lock coordinates writers?",
            resolutionLink: "section:data@resolution",
            actor: { id: null, name: "Deleted user" },
            decidedAt: RESOLVED_AT.toISOString(),
          },
        ],
      });
    },
  );

  test("serializes both decision kinds and their actors", async () => {
    const store: SpecDecisionStore = {
      async listPublishedDecisions(specId) {
        expect(specId).toBe(SPEC_ID);
        return [
          {
            id: SETTLE_ID,
            kind: "section_settled",
            sectionId: "data",
            sectionTitle: "Data model",
            actor: { id: SETTLE_ACTOR_ID, name: "Priya Raman" },
            decidedAt: SETTLED_AT,
          },
          {
            id: QUESTION_ID,
            kind: "question_resolved",
            sectionId: "data",
            sectionTitle: "Data model",
            question: "Which lock coordinates writers?",
            resolutionLink: "section:data@resolution",
            actor: { id: null, name: "Deleted user" },
            decidedAt: RESOLVED_AT,
          },
        ];
      },
    };

    const response = await memberRoute(store).request(`/api/v1/specs/${SPEC_ID}/decisions`);

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      decisions: [
        { kind: "section_settled", actor: { id: SETTLE_ACTOR_ID, name: "Priya Raman" } },
        { kind: "question_resolved", actor: { id: null, name: "Deleted user" } },
      ],
    });
  });

  test("rejects unauthenticated users and non-members before reading", async () => {
    let reads = 0;
    const store: SpecDecisionStore = {
      async listPublishedDecisions() {
        reads += 1;
        return [];
      },
    };
    const unauthenticated = makeSpecDecisionsRoute({
      store,
      resolveMembership: async () => true,
      getSession: async () => null,
    });
    const nonMember = memberRoute(store, "outsider", false);

    expect(
      (await unauthenticated.request(`/api/v1/specs/${SPEC_ID}/decisions`)).status,
    ).toBe(401);
    expect((await nonMember.request(`/api/v1/specs/${SPEC_ID}/decisions`)).status).toBe(404);
    expect(reads).toBe(0);
  });
});
