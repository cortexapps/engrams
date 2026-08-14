/**
 * The ticket tree against live Postgres.
 *
 * The assertion that carries this suite is that a proposal reads the *pinned*
 * checkpoint and never the live head. A fake reader cannot prove that, because
 * the fake is the thing under test. So here the document really moves on after
 * the pin, and the proposal has to keep seeing what was pinned.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { schema as documentSchema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import {
  PostgresPinnedSpecReader,
  PostgresSpecTicketStore,
  SpecTicketTreeService,
  type TicketProposalInput,
} from "../ticket-tree.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 4 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

const NOW = new Date("2026-08-12T15:04:00.000Z");

const TEMPLATE_SECTIONS = [
  {
    key: "data",
    title: "Data model",
    layerKey: "contract",
    guidance: "Give the data model.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
  {
    key: "api",
    title: "API",
    layerKey: "contract",
    guidance: "Give the API.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
];

/** A document with two sections, whose titles the caller chooses. */
function encodedDocument(sections: Array<{ id: string; key: string; title: string }>): Buffer {
  const document = documentSchema.nodes.doc!.create(
    null,
    sections.map((section) =>
      documentSchema.nodes.section!.create({ id: section.id, templateSectionKey: section.key }, [
        documentSchema.nodes.sectionHeading!.create(null, documentSchema.text(section.title)),
        documentSchema.nodes.paragraph!.create(null, [documentSchema.text("Body.")]),
      ]),
    ),
  );
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(document, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  const encoded = Buffer.from(Y.encodeStateAsUpdate(ydoc));
  ydoc.destroy();
  return encoded;
}

/** The pinned document: Data model and API. */
const PINNED_STATE = encodedDocument([
  { id: "sec-data", key: "data", title: "Data model" },
  { id: "sec-api", key: "api", title: "API" },
]);

/**
 * The head, moved on after the pin: one section renamed, one replaced. A
 * proposal that reads the head would resolve `sec-rollout` and would label
 * `sec-data` "Storage". A proposal that reads the pin does neither.
 */
const HEAD_STATE = encodedDocument([
  { id: "sec-data", key: "data", title: "Storage" },
  { id: "sec-rollout", key: "api", title: "Rollout" },
]);

const PROPOSAL: TicketProposalInput[] = [
  {
    client_id: "columns",
    title: "Add org quota columns",
    description: "Add the columns and backfill them.",
    section_id: "sec-data",
  },
  {
    client_id: "payload",
    parent_client_id: "columns",
    title: "Quota-aware 429 payload",
    description: "Return the remaining quota on a 429.",
    section_id: "sec-api",
  },
];

describe("the ticket tree with live Postgres", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const draftSpecId = randomUUID();
  const checkpointId = randomUUID();
  const headCheckpointId = randomUUID();
  const sessionId = randomUUID();
  const owner = `spec-ticket-owner-${randomUUID()}`;

  const service = () =>
    new SpecTicketTreeService({
      store: new PostgresSpecTicketStore(pool!),
      pinned: new PostgresPinnedSpecReader(pool!),
      newId: () => randomUUID(),
    });

  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Owner', $1 || '@example.test', false, now(), now())`,
      [owner],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections)
       VALUES ($1, 'Ticket tree test template', '[]', $2)`,
      [templateId, JSON.stringify(TEMPLATE_SECTIONS)],
    );
    for (const id of [specId, draftSpecId]) {
      await pool.query(
        `INSERT INTO spec (id, org_id, owner_user_id, session_id, template_id, title, phase)
         VALUES ($1, 'test-org', $2, $3, $4, 'Org sandbox quotas', 'drafting')`,
        [id, owner, sessionId, templateId],
      );
    }
    // The pin, and a later checkpoint that stands for the moved-on head.
    for (const [id, state, seq] of [
      [checkpointId, PINNED_STATE, "18"],
      [headCheckpointId, HEAD_STATE, "24"],
    ] as const) {
      await pool.query(
        `INSERT INTO spec_checkpoint
           (id, spec_id, state, state_vector, rendered_markdown, doc_seq, label, reason, created_at)
         VALUES ($1, $2, $3, $4, '# Org sandbox quotas', $5, 'Published version', 'publish', $6)`,
        [id, specId, state, Buffer.from([0]), seq, NOW],
      );
    }
    await pool.query(
      `UPDATE spec SET phase = 'published', published_checkpoint_id = $2,
              published_by = $3, published_at = $4, current_semantic_doc_seq = 24
        WHERE id = $1`,
      [specId, checkpointId, owner, NOW],
    );
    await pool.query(
      `INSERT INTO spec_open_question (id, spec_id, section_id, text, request_fingerprint, state)
       VALUES ($1, $2, 'sec-api', 'Does the 429 carry the reset time?', 'fp-1', 'open'),
              ($3, $2, 'sec-data', 'Answered already', 'fp-2', 'resolved')`,
      [randomUUID(), specId, randomUUID()],
    );
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [[specId, draftSpecId]]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    await pool.query('DELETE FROM "user" WHERE id = $1', [owner]);
    await pool.end();
  });

  test.skipIf(!reachable)("the proposal reads the pinned checkpoint, not the head", async () => {
    const { view } = await service().propose({
      specId,
      idempotencyKey: `pin-${randomUUID()}`,
      tickets: PROPOSAL,
    });

    // The pin is what the reader returned …
    expect(view.checkpointId).toBe(checkpointId);
    expect(view.docSeq).toBe("18");
    expect(view.sections.map((section) => section.id)).toEqual(["sec-data", "sec-api"]);

    // … so the backlink labels are the pinned titles, not the head's, and the
    // section the head added is not a section this proposal could have cited.
    const labels = view.tickets.map((ticket) => ticket.backlink.sectionTitle);
    expect(labels).toEqual(["Data model", "API"]);
    expect(labels).not.toContain("Storage");
    expect(view.sections.map((section) => section.id)).not.toContain("sec-rollout");
  });

  test.skipIf(!reachable)("a section only the head has cannot be cited", async () => {
    await expect(
      service().propose({
        specId,
        idempotencyKey: `head-${randomUUID()}`,
        tickets: [
          {
            client_id: "rollout",
            title: "Stage the rollout",
            description: "From a section the head added after the pin.",
            section_id: "sec-rollout",
          },
        ],
      }),
    ).rejects.toThrow("no section sec-rollout");
  });

  test.skipIf(!reachable)("a draft spec has no pin, so it cannot be ticketized", async () => {
    await expect(
      service().propose({
        specId: draftSpecId,
        idempotencyKey: `draft-${randomUUID()}`,
        tickets: PROPOSAL,
      }),
    ).rejects.toThrow("not published");
  });

  test.skipIf(!reachable)("the tree round-trips through the rows", async () => {
    const key = `store-${randomUUID()}`;
    const proposed = await service().propose({ specId, idempotencyKey: key, tickets: PROPOSAL });
    const parent = proposed.view.tickets[0];
    const child = proposed.view.tickets[1];

    const rows = await pool!.query<{
      id: string;
      parent_id: string | null;
      ordinal: number;
      section_id: string;
      depends_on: string[];
      sync_state: string;
      linear_id: string | null;
      sync_error: string | null;
    }>(
      `SELECT id, parent_id, ordinal, section_id, depends_on, sync_state, linear_id, sync_error
         FROM spec_ticket_draft WHERE spec_id = $1
        ORDER BY parent_id NULLS FIRST, ordinal`,
      [specId],
    );
    expect(rows.rows).toHaveLength(2);
    expect(rows.rows[0]?.id).toBe(parent?.id ?? "");
    expect(rows.rows[0]?.parent_id).toBeNull();
    expect(rows.rows[1]?.id).toBe(child?.id ?? "");
    expect(rows.rows[1]?.parent_id).toBe(parent?.id ?? "");
    // Both are first among their own siblings, so both ordinals are 0.
    expect(rows.rows.map((row) => row.ordinal)).toEqual([0, 0]);
    // The sync columns belong to #1128. This change only creates them.
    expect(rows.rows.map((row) => row.sync_state)).toEqual(["draft", "draft"]);
    expect(rows.rows.map((row) => row.linear_id)).toEqual([null, null]);
    expect(rows.rows.map((row) => row.sync_error)).toEqual([null, null]);

    const read = await service().read(specId);
    expect(read.tickets.map((ticket) => ticket.id)).toEqual([parent?.id ?? "", child?.id ?? ""]);
    expect(read.tickets[1]?.depth).toBe(1);
  });

  test.skipIf(!reachable)("only the unresolved questions ride on the tree", async () => {
    await service().propose({
      specId,
      idempotencyKey: `questions-${randomUUID()}`,
      tickets: PROPOSAL,
    });
    const view = await service().read(specId);
    const api = view.tickets.find((ticket) => ticket.backlink.sectionId === "sec-api");
    const data = view.tickets.find((ticket) => ticket.backlink.sectionId === "sec-data");
    expect(api?.openQuestions.map((question) => question.text)).toEqual([
      "Does the 429 carry the reset time?",
    ]);
    expect(data?.openQuestions).toEqual([]);
    expect(view.unattachedQuestions).toEqual([]);
  });

  test.skipIf(!reachable)("a drag to nest and a merge survive the write", async () => {
    const proposed = await service().propose({
      specId,
      idempotencyKey: `move-${randomUUID()}`,
      tickets: PROPOSAL,
    });
    const parent = proposed.view.tickets[0]?.id ?? "";
    const child = proposed.view.tickets[1]?.id ?? "";

    // Drag the child out to the root, in front of its old parent.
    const moved = await service().moveTicket({ specId, id: child, parentId: null, index: 0 });
    expect(moved.tickets.map((ticket) => ticket.id)).toEqual([child, parent]);
    expect(moved.tickets.map((ticket) => ticket.ordinal)).toEqual([0, 1]);
    expect(await storedParent(child)).toBeNull();

    // Then fold it back into its old parent. The parent survives, alone.
    const merged = await service().mergeTickets({ specId, targetId: parent, sourceIds: [child] });
    expect(merged.tickets.map((ticket) => ticket.id)).toEqual([parent]);
    const remaining = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_ticket_draft WHERE spec_id = $1",
      [specId],
    );
    expect(remaining.rows[0]?.count).toBe(1);
    expect(merged.tickets[0]?.description.match(/§/g)).toHaveLength(1);
  });

  test.skipIf(!reachable)("a delete takes the subtree out of the rows", async () => {
    const proposed = await service().propose({
      specId,
      idempotencyKey: `delete-${randomUUID()}`,
      tickets: PROPOSAL,
    });
    const parent = proposed.view.tickets[0]?.id ?? "";
    const view = await service().deleteTicket({ specId, id: parent });
    expect(view.tickets).toEqual([]);
    const remaining = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_ticket_draft WHERE spec_id = $1",
      [specId],
    );
    expect(remaining.rows[0]?.count).toBe(0);
  });

  test.skipIf(!reachable)("a replayed proposal writes one tree", async () => {
    const key = `replay-${randomUUID()}`;
    const first = await service().propose({ specId, idempotencyKey: key, tickets: PROPOSAL });
    expect(first.applied).toBe(true);
    const second = await service().propose({ specId, idempotencyKey: key, tickets: PROPOSAL });
    expect(second.applied).toBe(false);
    const rows = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_ticket_draft WHERE spec_id = $1",
      [specId],
    );
    expect(rows.rows[0]?.count).toBe(2);
  });

  test.skipIf(!reachable)("a retitle writes one row and leaves the rest untouched", async () => {
    const proposed = await service().propose({
      specId,
      idempotencyKey: `retitle-${randomUUID()}`,
      tickets: PROPOSAL,
    });
    const target = proposed.view.tickets[0]?.id ?? "";
    const other = proposed.view.tickets[1]?.id ?? "";
    const untouchedBefore = await rowVersion(other);

    await service().updateTicket({ specId, id: target, title: "Add org quota columns + backfill" });

    // `xmin` is the transaction that last wrote the row. A write proportional
    // to the change cannot have touched a row the change did not name.
    expect(await rowVersion(other)).toBe(untouchedBefore);
    const titles = (await service().read(specId)).tickets.map((ticket) => ticket.title);
    expect(titles[0]).toBe("Add org quota columns + backfill");
  });

  test.skipIf(!reachable)("folding a parent keeps the children the merge re-parented", async () => {
    // `parent_id` cascades on delete, so a merge that removes a ticket whose
    // children the survivor adopts is the case that must not lose them.
    const proposed = await service().propose({
      specId,
      idempotencyKey: `cascade-${randomUUID()}`,
      tickets: PROPOSAL,
    });
    const parent = proposed.view.tickets[0]?.id ?? "";
    const child = proposed.view.tickets[1]?.id ?? "";
    expect(await storedParent(child)).toBe(parent);

    const added = await service().addTicket({
      specId,
      parentId: child,
      title: "Backfill in batches",
      description: "The batches.",
      sectionId: "sec-data",
    });
    const grandchild = added.tickets.find((ticket) => ticket.title === "Backfill in batches")?.id;
    if (grandchild === undefined) throw new Error("the added ticket is missing");

    // Fold the middle ticket into the root. Its child must survive, under the
    // survivor, rather than disappearing with its old parent.
    const merged = await service().mergeTickets({ specId, targetId: parent, sourceIds: [child] });
    expect(merged.tickets.map((ticket) => ticket.id)).not.toContain(child);
    expect(merged.tickets.map((ticket) => ticket.id)).toContain(grandchild);
    expect(await storedParent(grandchild)).toBe(parent);
    const rows = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_ticket_draft WHERE spec_id = $1",
      [specId],
    );
    expect(rows.rows[0]?.count).toBe(2);
  });

  async function rowVersion(id: string): Promise<string> {
    const result = await pool!.query<{ version: string }>(
      "SELECT xmin::text AS version FROM spec_ticket_draft WHERE id = $1",
      [id],
    );
    return result.rows[0]?.version ?? "missing";
  }

  async function storedParent(id: string): Promise<string | null> {
    const result = await pool!.query<{ parent_id: string | null }>(
      "SELECT parent_id FROM spec_ticket_draft WHERE id = $1",
      [id],
    );
    return result.rows[0]?.parent_id ?? null;
  }
});
