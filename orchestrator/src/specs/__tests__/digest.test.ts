import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createTemplateDocument, type SpecTemplate } from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";
import * as Y from "yjs";

import { encodeProseMirrorDocument } from "../doc-service.ts";
import { PostgresSpecDigestSource, SpecDigestService } from "../digest.ts";

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

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 2 }) : null;
let reachable = false;
if (pool) reachable = await pool.query("SELECT 1").then(() => true).catch(() => false);

describe("SpecDigestService with live Postgres", () => {
  const templateId = randomUUID();
  const firstSpec = randomUUID();
  const secondSpec = randomUUID();
  const snapshotBaseSpec = randomUUID();
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
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Digest test', '[]', '[]', '{}')`,
      [templateId],
    );
    await pool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle, current_doc_seq)
       VALUES ($1, 'test', $3, 'First', 'draft', 2),
              ($2, 'test', $3, 'Second', 'draft', 2),
              ($4, 'test', $3, 'Snapshot base', 'draft', 1)`,
      [firstSpec, secondSpec, templateId, snapshotBaseSpec],
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
      `INSERT INTO spec_update_log (spec_id, seq, update, client_id)
       VALUES ($1, 1, $3, 'seed'), ($1, 2, $4, 'human'),
              ($2, 1, $5, 'seed'), ($2, 2, $6, 'human')`,
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
      `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq)
       VALUES ($1, $2, $3, 2)`,
      [
        firstSpec,
        Buffer.from(Y.encodeStateAsUpdate(compacted)),
        Buffer.from(Y.encodeStateVector(compacted)),
      ],
    );
    await pool.query(
      `INSERT INTO spec_projection
         (spec_id, rev, session_id, doc_seq, sha256, rendered, document_state,
          digest, digest_sha256, staging_path, state, requested_source,
          pushed_at, created_at)
       VALUES ($1, 1, $1, 1, 'base', ''::bytea, $2,
               ''::bytea, 'digest', '/workspace/.engrams/spec/incoming-1.md',
               'published', 'test', $3, $3)`,
      [firstSpec, Buffer.from(firstInitial), now],
    );
    const [snapshotBase, snapshotDelta] = updates("from snapshot");
    const snapshotDoc = new Y.Doc();
    Y.applyUpdate(snapshotDoc, snapshotBase);
    await pool.query(
      `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq)
       VALUES ($1, $2, $3, 0)`,
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
      `INSERT INTO spec_update_log (spec_id, seq, update, client_id)
       VALUES ($1, 1, $2, 'human')`,
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
    expect(digest).toContain("Ada updated this section");
    expect(digest).not.toContain("Grace");
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
