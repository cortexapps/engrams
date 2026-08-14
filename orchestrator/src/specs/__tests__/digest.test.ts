import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createTemplateDocument, type SpecTemplate } from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";
import * as Y from "yjs";

import { encodeProseMirrorDocument } from "../doc-service.ts";
import {
  PostgresSpecDigestSource,
  SPEC_DIGEST_MAX_BYTES,
  SpecDigestService,
  type SpecDigestSnapshot,
  type SpecDigestSource,
} from "../digest.ts";

const TEMPLATE: SpecTemplate = {
  sections: [{ id: "context", key: "context", title: "Context" }],
};

function updates(value: string): [Uint8Array, Uint8Array] {
  const initial = encodeProseMirrorDocument(createTemplateDocument(TEMPLATE));
  const doc = new Y.Doc();
  Y.applyUpdate(doc, initial);
  const vector = Y.encodeStateVector(doc);
  const section = doc.getXmlFragment("prosemirror").get(0);
  if (!(section instanceof Y.XmlElement)) throw new Error("missing test section");
  const paragraph = section.get(1);
  if (!(paragraph instanceof Y.XmlElement)) throw new Error("missing test paragraph");
  const text = new Y.XmlText();
  text.insert(0, value);
  paragraph.insert(0, [text]);
  return [initial, Y.encodeStateAsUpdate(doc, vector)];
}

function memorySource(snapshot: SpecDigestSnapshot): SpecDigestSource {
  return { read: async () => snapshot };
}

describe("SpecDigestService rendering", () => {
  test("names the phase, section status, settle credit, presence, and question counts", async () => {
    const digest = await new SpecDigestService(
      memorySource({
        phase: "drafting",
        sections: [
          {
            sectionId: "problem",
            sectionTitle: "Problem",
            state: "settled",
            stateChangedAt: new Date("2026-08-13T18:42:00.000Z"),
            settledBy: "Priya Shah",
            openQuestionCount: 0,
          },
          {
            sectionId: "design",
            sectionTitle: "Proposed design",
            state: "proposed",
            stateChangedAt: new Date("2026-08-13T19:03:00.000Z"),
            settledBy: null,
            openQuestionCount: 2,
          },
          {
            sectionId: "rollout",
            sectionTitle: "Rollout",
            state: "open",
            stateChangedAt: null,
            settledBy: null,
            openQuestionCount: 1,
          },
        ],
        changes: [
          { sectionId: "problem", sectionTitle: "Problem", author: "Nikhil Unni" },
          { sectionId: "design", sectionTitle: "Proposed design", author: "Marcus Lee" },
        ],
      }),
    ).render("spec-1", 3n, 5n, false, null, ["Nikhil Unni", "Priya Shah"]);

    expect(digest).toContain("Phase: drafting.");
    expect(digest).toContain(
      "Problem: settled by Priya Shah at 2026-08-13T18:42:00.000Z.",
    );
    expect(digest).toContain("Proposed design: proposed since 2026-08-13T19:03:00.000Z.");
    expect(digest).toContain("Nikhil Unni updated this section.");
    expect(digest).toContain("## People here now\nNikhil Unni, Priya Shah.");
    expect(digest).toContain("Problem: 0 open questions.");
    expect(digest).toContain("Proposed design: 2 open questions.");
    expect(digest).toContain("Rollout: 1 open question.");
  });

  test("keeps load-bearing settlement context within its own byte budget", async () => {
    const long = "界".repeat(100);
    const sections = Array.from({ length: 120 }, (_, index) => ({
      sectionId: `section-${index}`,
      sectionTitle: `Section ${index} ${long}`,
      state: index === 119 ? ("settled" as const) : ("open" as const),
      stateChangedAt: index === 119 ? new Date("2026-08-13T20:15:00.000Z") : null,
      settledBy: index === 119 ? `Alexandra ${long}` : null,
      openQuestionCount: index % 4,
    }));
    const digest = await new SpecDigestService(
      memorySource({
        phase: "drafting",
        sections,
        changes: sections.map((section, index) => ({
          sectionId: section.sectionId,
          sectionTitle: section.sectionTitle,
          author: `Collaborator ${index} ${long}`,
        })),
      }),
    ).render("spec-large", 0n, 120n, false, null, [`Present ${long}`]);

    expect(new TextEncoder().encode(digest).byteLength).toBeLessThanOrEqual(
      SPEC_DIGEST_MAX_BYTES,
    );
    expect(digest).toContain("Phase: drafting.");
    expect(digest).toContain("settled by Alexandra");
    expect(digest).toContain("at 2026-08-13T20:15:00.000Z");
  });
});

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 2 }) : null;
let reachable = false;
if (pool) reachable = await pool.query("SELECT 1").then(() => true).catch(() => false);

describe("SpecDigestService with live Postgres", () => {
  const templateId = randomUUID();
  const firstSpec = randomUUID();
  const secondSpec = randomUUID();
  const snapshotBaseSpec = randomUUID();
  const openQuestionId = randomUUID();
  const firstUser = `digest-${randomUUID()}`;
  const secondUser = `digest-${randomUUID()}`;
  let firstBaseState: Uint8Array = new Uint8Array();

  beforeAll(async () => {
    if (!reachable || !pool) return;
    const now = new Date("2026-08-09T12:00:00.000Z");
    const participantLeaseExpiresAt = new Date("2100-01-01T00:00:00.000Z");
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Ada', $2, false, $3, $3),
              ($4, 'Grace', $5, false, $3, $3)`,
      [firstUser, `${firstUser}@example.test`, now, secondUser, `${secondUser}@example.test`],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections)
       VALUES ($1, 'Digest test', '[]', '[]')`,
      [templateId],
    );
    await pool.query(
      `INSERT INTO spec
         (id, org_id, template_id, title, phase, current_doc_seq, current_semantic_doc_seq)
       VALUES ($1, 'test', $3, 'First', 'drafting', 2, 2),
              ($2, 'test', $3, 'Second', 'drafting', 2, 2),
              ($4, 'test', $3, 'Snapshot base', 'drafting', 1, 1)`,
      [firstSpec, secondSpec, templateId, snapshotBaseSpec],
    );
    await pool.query(
      `INSERT INTO spec_section_state
         (spec_id, section_id, state, na_reason, settled_by, updated_at)
       VALUES ($1, 'context', 'settled', NULL, $2, $3)`,
      [firstSpec, firstUser, now],
    );
    await pool.query(
      `INSERT INTO spec_open_question
         (id, spec_id, section_id, text, opened_by, request_fingerprint, state, created_at)
       VALUES ($1, $2, 'context', 'Which rollout tier goes first?', $3, 'digest-test', 'open', $4)`,
      [openQuestionId, firstSpec, secondUser, now],
    );
    const [firstInitial, firstEdit] = updates("first");
    firstBaseState = firstInitial;
    const [secondInitial, secondEdit] = updates("second");
    await pool.query(
      `INSERT INTO spec_participant
         (spec_id, client_id, user_id, connection_epoch, connected_at, lease_expires_at)
       VALUES ($1, 'human', $3, 1, $5, $6), ($2, 'human', $4, 1, $5, $6)`,
      [firstSpec, secondSpec, firstUser, secondUser, now, participantLeaseExpiresAt],
    );
    await pool.query(
      `INSERT INTO spec_update_log (spec_id, seq, semantic_doc_seq, update, client_id)
       VALUES ($1, 1, 1, $3, 'seed'), ($1, 2, 2, $4, 'human'),
              ($2, 1, 1, $5, 'seed'), ($2, 2, 2, $6, 'human')`,
      [
        firstSpec,
        secondSpec,
        Buffer.from(firstInitial),
        Buffer.from(firstEdit),
        Buffer.from(secondInitial),
        Buffer.from(secondEdit),
      ],
    );
    const compacted = new Y.Doc();
    Y.applyUpdate(compacted, firstInitial);
    Y.applyUpdate(compacted, firstEdit);
    await pool.query(
      `INSERT INTO spec_snapshot
         (spec_id, state, state_vector, covered_seq, covered_semantic_doc_seq)
       VALUES ($1, $2, $3, 2, 2)`,
      [
        firstSpec,
        Buffer.from(Y.encodeStateAsUpdate(compacted)),
        Buffer.from(Y.encodeStateVector(compacted)),
      ],
    );
    await pool.query(
      `INSERT INTO spec_projection
         (spec_id, rev, session_id, doc_seq, semantic_doc_seq, sha256, rendered, document_state,
          digest, digest_sha256, staging_path, state, requested_source,
          pushed_at, created_at)
       VALUES ($1, 1, $1, 1, 1, 'base', ''::bytea, $2,
               ''::bytea, 'digest', '/workspace/.engrams/spec/incoming-1.md',
               'published', 'test', $3, $3)`,
      [firstSpec, Buffer.from(firstInitial), now],
    );
    const [snapshotBase, snapshotDelta] = updates("from snapshot");
    const snapshotDoc = new Y.Doc();
    Y.applyUpdate(snapshotDoc, snapshotBase);
    await pool.query(
      `INSERT INTO spec_snapshot
         (spec_id, state, state_vector, covered_seq, covered_semantic_doc_seq)
       VALUES ($1, $2, $3, 0, 0)`,
      [
        snapshotBaseSpec,
        Buffer.from(snapshotBase),
        Buffer.from(Y.encodeStateVector(snapshotDoc)),
      ],
    );
    await pool.query(
      `INSERT INTO spec_participant
         (spec_id, client_id, user_id, connection_epoch, connected_at, lease_expires_at)
       VALUES ($1, 'human', $2, 1, $3, $4)`,
      [snapshotBaseSpec, firstUser, now, participantLeaseExpiresAt],
    );
    await pool.query(
      `INSERT INTO spec_update_log (spec_id, seq, semantic_doc_seq, update, client_id)
       VALUES ($1, 1, 1, $2, 'human')`,
      [snapshotBaseSpec, Buffer.from(snapshotDelta)],
    );
  }, 15_000);

  afterAll(async () => {
    if (!pool) return;
    if (reachable) {
      await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [[
        firstSpec,
        secondSpec,
        snapshotBaseSpec,
      ]]);
      await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
      await pool.query('DELETE FROM "user" WHERE id = ANY($1::text[])', [[firstUser, secondUser]]);
    }
    await pool.end();
    pool = null;
  }, 15_000);

  test.skipIf(!reachable)("uses the published state when compaction is ahead and excludes another spec", async () => {
    const digest = await new SpecDigestService(new PostgresSpecDigestSource()).render(
      firstSpec,
      1n,
      2n,
      false,
      firstBaseState,
    );
    expect(digest).toContain("Phase: drafting.");
    expect(digest).toContain("Context: settled by Ada at 2026-08-09T12:00:00.000Z.");
    expect(digest).toContain("Context: 1 open question.");
    expect(digest).toContain("Ada updated this section");
    expect(digest).not.toContain("Grace updated this section");
    expect(digest).not.toContain("second");
  });

  test.skipIf(!reachable)("uses a seq-zero snapshot for the first dependent human delta", async () => {
    const digest = await new SpecDigestService(new PostgresSpecDigestSource()).render(
      snapshotBaseSpec,
      0n,
      1n,
      false,
    );
    expect(digest).toContain("Ada updated this section");
    expect(digest).toContain("Context");
  });
});
