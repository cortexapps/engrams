/**
 * The ticket tree service (ADR 0114 D6, R29, R39-R40).
 *
 * After publish the canvas becomes a tree of proposed tickets. Two things
 * write it: the agent, once, through `spec_propose_tickets`, and a person,
 * repeatedly, through direct manipulation. This module owns both.
 *
 * The rule that carries the feature is that a proposal reads the **pinned**
 * checkpoint and never the live head. A published spec cannot be revised
 * (R38), so the head is the pin; but the read must still go through the pin,
 * because that is what makes every backlink resolvable for the life of the
 * spec. `PinnedSpecReader` is therefore the only door to the document here —
 * this module never touches `SpecDocumentService`.
 *
 * The tree math itself is pure and shared with the browser
 * (`@engrams/spec-document`). What is left here is the pinned read, the
 * backlink, the open-question attachment and the transaction.
 */

import {
  addTicket,
  backlinkBody,
  deleteTicket,
  mergeTickets,
  moveTicket,
  normalizeTree,
  orderedTree,
  schema,
  splitTicket,
  specTicketBacklink,
  SpecTicketTreeError,
  treeDepths,
  updateTicket,
  withBacklink,
  type AddTicketInput,
  type MergeTicketsInput,
  type MoveTicketInput,
  type SpecTicketBacklink,
  type SpecTicketNode,
  type SplitPart,
  type UpdateTicketInput,
} from "@engrams/spec-document";
import { createHash } from "node:crypto";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import type { Pool, PoolClient } from "pg";
import * as Y from "yjs";

import { proseMirrorDocument } from "./doc-service.ts";

/** One section of the pinned spec, as a backlink target. */
export interface PinnedSection {
  id: string;
  title: string;
}

/** An open question the owner published with (R35), still unanswered. */
export interface CarriedQuestion {
  id: string;
  sectionId: string;
  text: string;
}

/** The pinned spec: the only document a proposal is allowed to read. */
export interface PinnedSpec {
  specId: string;
  checkpointId: string;
  /** The pin's own revision, for the "spec vN pinned" chip on the canvas. */
  docSeq: bigint;
  publishedAt: Date | null;
  sections: PinnedSection[];
  openQuestions: CarriedQuestion[];
}

export interface PinnedSpecReader {
  /** Null when the spec is not published, so there is nothing to propose from. */
  read(specId: string): Promise<PinnedSpec | null>;
}

/** A stored draft. The shape is the row, plus the spec it belongs to. */
export interface SpecTicketDraftRecord extends SpecTicketNode {
  specId: string;
}

export interface SpecTicketStore {
  list(specId: string): Promise<SpecTicketDraftRecord[]>;
  /**
   * Lock the spec, hand the current tree to `apply`, and write what it
   * returns. The lock is what makes a drag from one browser and a proposal
   * from the agent serialise instead of overwriting each other.
   */
  mutate(
    specId: string,
    apply: (current: SpecTicketDraftRecord[]) => SpecTicketDraftRecord[],
  ): Promise<SpecTicketDraftRecord[]>;
}

export type SpecTicketErrorCode = "not_published" | "unknown_section" | "not_found" | "invalid";

export class SpecTicketError extends Error {
  constructor(
    readonly code: SpecTicketErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "SpecTicketError";
  }
}

/** One ticket as the canvas reads it: the row, plus what it links to. */
export interface SpecTicketView {
  id: string;
  parentId: string | null;
  ordinal: number;
  depth: number;
  title: string;
  /** The description without its opening backlink line — what a person edits. */
  body: string;
  /** The stored description, backlink line included. */
  description: string;
  backlink: SpecTicketBacklink;
  dependsOn: string[];
  syncState: SpecTicketNode["syncState"];
  linearId: string | null;
  syncError: string | null;
  /** The unresolved questions covering this ticket's section (R29). */
  openQuestions: CarriedQuestion[];
}

/** The whole surface the ticket tab renders. */
export interface SpecTicketTreeView {
  specId: string;
  checkpointId: string;
  docSeq: string;
  publishedAt: Date | null;
  sections: PinnedSection[];
  tickets: SpecTicketView[];
  /**
   * Questions whose section no ticket covers. Dropping one silently is the
   * trust problem R29 exists to prevent, so the tree reports them instead.
   */
  unattachedQuestions: CarriedQuestion[];
}

/** One ticket of an agent proposal, as the tool delivers it. */
export interface TicketProposalInput {
  client_id: string;
  parent_client_id?: string | undefined;
  title: string;
  description: string;
  section_id: string;
  depends_on?: string[] | undefined;
}

export interface ProposeTicketsInput {
  specId: string;
  /** Makes a replayed tool call land one tree, not two. */
  idempotencyKey: string;
  tickets: readonly TicketProposalInput[];
}

export interface ProposeTicketsResult {
  view: SpecTicketTreeView;
  /** False when this proposal had already landed, so nothing changed. */
  applied: boolean;
}

export interface SpecTicketTreeServiceOptions {
  store: SpecTicketStore;
  pinned: PinnedSpecReader;
  newId: () => string;
}

export class SpecTicketTreeService {
  constructor(private readonly options: SpecTicketTreeServiceOptions) {}

  /** The tree as the canvas reads it, with backlinks and questions resolved. */
  async read(specId: string): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(specId);
    return this.view(pinned, await this.options.store.list(specId));
  }

  /**
   * Replace the tree with the agent's proposal.
   *
   * Every section id is checked against the pinned document, so a proposal
   * that cites a section the pinned spec does not have is refused whole. A
   * half-written tree with one dead backlink is worse than no tree.
   */
  async propose(input: ProposeTicketsInput): Promise<ProposeTicketsResult> {
    const pinned = await this.requirePinned(input.specId);
    if (input.tickets.length === 0) {
      throw new SpecTicketError("invalid", "A proposal must name at least one ticket.");
    }

    const sections = new Map(pinned.sections.map((section) => [section.id, section]));
    const unknown = input.tickets
      .map((ticket) => ticket.section_id)
      .filter((sectionId) => !sections.has(sectionId));
    if (unknown.length > 0) {
      throw new SpecTicketError(
        "unknown_section",
        `The pinned spec has no section ${[...new Set(unknown)].join(", ")}.`,
      );
    }

    // The row id is derived from the proposal, so a replay recomputes the same
    // ids and the tree it would write is the tree already there.
    const idByClientId = new Map(
      input.tickets.map((ticket) => [
        ticket.client_id,
        proposalDraftId(input.specId, input.idempotencyKey, ticket.client_id),
      ]),
    );

    const proposed: SpecTicketDraftRecord[] = input.tickets.map((ticket, index) => {
      const section = sections.get(ticket.section_id);
      const id = idByClientId.get(ticket.client_id);
      // Both lookups were populated from `input.tickets` a few lines above,
      // and the unknown-section check ran first.
      if (!section || !id) throw new SpecTicketError("invalid", "The proposal is inconsistent.");
      const backlink = specTicketBacklink(input.specId, section);
      const parentClientId = ticket.parent_client_id;
      return {
        id,
        specId: input.specId,
        parentId:
          parentClientId === undefined ? null : (idByClientId.get(parentClientId) ?? null),
        ordinal: index,
        title: ticket.title,
        description: withBacklink(ticket.description, backlink),
        sectionId: section.id,
        dependsOn: (ticket.depends_on ?? [])
          .map((clientId) => idByClientId.get(clientId))
          .filter((id): id is string => id !== undefined),
        syncState: "draft",
        linearId: null,
        syncError: null,
      };
    });

    let applied = true;
    const stored = await this.options.store.mutate(input.specId, (current) => {
      // A replay of the same proposal keeps the tree a person may have edited
      // since. Any one id already present proves the proposal landed.
      const landed = new Set(current.map((row) => row.id));
      if (proposed.some((row) => landed.has(row.id))) {
        applied = false;
        return current;
      }
      return normalizeTree(proposed).map((node) => ({ ...node, specId: input.specId }));
    });
    return { view: this.view(pinned, stored), applied };
  }

  async addTicket(input: {
    specId: string;
    parentId: string | null;
    index?: number | undefined;
    title: string;
    description: string;
    sectionId: string;
  }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    const backlink = this.requireBacklink(pinned, input.sectionId);
    const added: AddTicketInput = {
      id: this.options.newId(),
      parentId: input.parentId,
      ...(input.index === undefined ? {} : { index: input.index }),
      title: input.title,
      description: withBacklink(input.description, backlink),
      sectionId: input.sectionId,
    };
    return this.apply(pinned, (current) => addTicket(current, added));
  }

  /**
   * Retitle, rewrite the body, or re-point the backlink. The caller sends the
   * body a person typed; the service writes the link line back on, so a
   * hand-edited description can never lose its backlink.
   */
  async updateTicket(input: {
    specId: string;
    id: string;
    title?: string | undefined;
    body?: string | undefined;
    sectionId?: string | undefined;
    dependsOn?: string[] | undefined;
  }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    return this.apply(pinned, (current) => {
      const existing = current.find((row) => row.id === input.id);
      if (!existing) throw new SpecTicketError("not_found", `Unknown ticket: ${input.id}`);
      const sectionId = input.sectionId ?? existing.sectionId;
      const backlink = this.requireBacklink(pinned, sectionId);
      const body = input.body ?? backlinkBody(existing.description);
      const update: UpdateTicketInput = {
        id: input.id,
        ...(input.title === undefined ? {} : { title: input.title }),
        description: withBacklink(body, backlink),
        sectionId,
        ...(input.dependsOn === undefined ? {} : { dependsOn: input.dependsOn }),
      };
      return updateTicket(current, update);
    });
  }

  async deleteTicket(input: { specId: string; id: string }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    return this.apply(pinned, (current) => deleteTicket(current, input.id));
  }

  /** Drag to nest, and drag to reorder: one gesture, one verb. */
  async moveTicket(input: {
    specId: string;
    id: string;
    parentId: string | null;
    index?: number | undefined;
  }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    const move: MoveTicketInput = {
      id: input.id,
      parentId: input.parentId,
      ...(input.index === undefined ? {} : { index: input.index }),
    };
    return this.apply(pinned, (current) => moveTicket(current, move));
  }

  async splitTicket(input: {
    specId: string;
    id: string;
    parts: ReadonlyArray<{ title: string; body: string; sectionId?: string | undefined }>;
  }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    return this.apply(pinned, (current) => {
      const source = current.find((row) => row.id === input.id);
      if (!source) throw new SpecTicketError("not_found", `Unknown ticket: ${input.id}`);
      const parts: SplitPart[] = input.parts.map((part, index) => {
        const sectionId = part.sectionId ?? source.sectionId;
        const backlink = this.requireBacklink(pinned, sectionId);
        return {
          id: index === 0 ? source.id : this.options.newId(),
          title: part.title,
          description: withBacklink(part.body, backlink),
          sectionId,
        };
      });
      return splitTicket(current, input.id, parts);
    });
  }

  /** Fold tickets into one. The survivor keeps its place and its backlink. */
  async mergeTickets(input: {
    specId: string;
    targetId: string;
    sourceIds: readonly string[];
    title?: string | undefined;
    body?: string | undefined;
  }): Promise<SpecTicketTreeView> {
    const pinned = await this.requirePinned(input.specId);
    return this.apply(pinned, (current) => {
      const target = current.find((row) => row.id === input.targetId);
      if (!target) throw new SpecTicketError("not_found", `Unknown ticket: ${input.targetId}`);
      const backlink = this.requireBacklink(pinned, target.sectionId);
      // Each part carries its own opening link. Join the bodies, then write
      // the survivor's one link back on, so a merge of three tickets does not
      // produce a description that starts with three backlinks.
      const folded = input.sourceIds
        .filter((id) => id !== input.targetId)
        .map((id) => current.find((row) => row.id === id))
        .filter((row): row is SpecTicketDraftRecord => row !== undefined);
      const body =
        input.body ??
        [target, ...folded]
          .map((row) => backlinkBody(row.description))
          .filter((part) => part.length > 0)
          .join("\n\n");
      const merge: MergeTicketsInput = {
        targetId: input.targetId,
        sourceIds: input.sourceIds,
        ...(input.title === undefined ? {} : { title: input.title }),
        description: withBacklink(body, backlink),
      };
      return mergeTickets(current, merge);
    });
  }

  private async apply(
    pinned: PinnedSpec,
    change: (current: SpecTicketDraftRecord[]) => SpecTicketNode[],
  ): Promise<SpecTicketTreeView> {
    const stored = await this.options.store.mutate(pinned.specId, (current) =>
      change(current).map((node) => ({ ...node, specId: pinned.specId })),
    );
    return this.view(pinned, stored);
  }

  private async requirePinned(specId: string): Promise<PinnedSpec> {
    const pinned = await this.options.pinned.read(specId);
    if (!pinned) {
      throw new SpecTicketError(
        "not_published",
        `Spec ${specId} is not published, so it has no pinned version to ticketize.`,
      );
    }
    return pinned;
  }

  private requireBacklink(pinned: PinnedSpec, sectionId: string): SpecTicketBacklink {
    const section = pinned.sections.find((candidate) => candidate.id === sectionId);
    if (!section) {
      throw new SpecTicketError(
        "unknown_section",
        `The pinned spec has no section ${sectionId}.`,
      );
    }
    return specTicketBacklink(pinned.specId, section);
  }

  private view(pinned: PinnedSpec, rows: readonly SpecTicketDraftRecord[]): SpecTicketTreeView {
    const ordered = orderedTree(rows);
    const depths = treeDepths(rows);
    const titles = new Map(pinned.sections.map((section) => [section.id, section.title]));
    const covered = new Set(ordered.map((row) => row.sectionId));
    const tickets = ordered.map((row) => {
      const section = { id: row.sectionId, title: titles.get(row.sectionId) ?? row.sectionId };
      return {
        id: row.id,
        parentId: row.parentId,
        ordinal: row.ordinal,
        depth: depths.get(row.id) ?? 0,
        title: row.title,
        body: backlinkBody(row.description),
        description: row.description,
        backlink: specTicketBacklink(pinned.specId, section),
        dependsOn: row.dependsOn,
        syncState: row.syncState,
        linearId: row.linearId,
        syncError: row.syncError,
        // A question rides on every ticket that covers its section, so a
        // re-parent or a merge cannot shake it off (R29).
        openQuestions: pinned.openQuestions.filter(
          (question) => question.sectionId === row.sectionId,
        ),
      };
    });
    return {
      specId: pinned.specId,
      checkpointId: pinned.checkpointId,
      docSeq: pinned.docSeq.toString(),
      publishedAt: pinned.publishedAt,
      sections: pinned.sections,
      tickets,
      unattachedQuestions: pinned.openQuestions.filter(
        (question) => !covered.has(question.sectionId),
      ),
    };
  }
}

/** The stable row id of one proposed ticket. */
export function proposalDraftId(
  specId: string,
  idempotencyKey: string,
  clientId: string,
): string {
  return uuidV5(`${specId}:${idempotencyKey}:${clientId}`, TICKET_DRAFT_NAMESPACE);
}

const TICKET_DRAFT_NAMESPACE = "0d3a5f26-9c4a-5a1b-9a3d-6d5f0f7b2c81";

function uuidV5(name: string, namespace: string): string {
  const hash = createHash("sha1");
  hash.update(Buffer.from(namespace.replace(/-/g, ""), "hex"));
  hash.update(Buffer.from(name, "utf8"));
  const bytes = hash.digest().subarray(0, 16);
  bytes[6] = ((bytes[6] as number) & 0x0f) | 0x50;
  bytes[8] = ((bytes[8] as number) & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

interface PinnedRow {
  checkpoint_id: string;
  doc_seq: string;
  published_at: Date | null;
  state: Buffer;
}

interface QuestionRow {
  id: string;
  section_id: string;
  text: string;
}

/**
 * Reads the pinned checkpoint, and only the pinned checkpoint.
 *
 * The join is `spec.published_checkpoint_id` → `spec_checkpoint`. It never
 * reads `spec_snapshot` or `spec_update_log`, which is what makes "the
 * proposal comes from the pinned spec" a property of the query and not of a
 * comment.
 */
export class PostgresPinnedSpecReader implements PinnedSpecReader {
  constructor(private readonly pool: Pool) {}

  async read(specId: string): Promise<PinnedSpec | null> {
    const pinned = await this.pool.query<PinnedRow>(
      `SELECT c.id AS checkpoint_id, c.doc_seq, s.published_at, c.state
         FROM spec s
         JOIN spec_checkpoint c
           ON c.id = s.published_checkpoint_id AND c.spec_id = s.id
        WHERE s.id = $1 AND s.lifecycle = 'published'`,
      [specId],
    );
    const row = pinned.rows[0];
    if (!row) return null;

    const questions = await this.pool.query<QuestionRow>(
      `SELECT id, section_id, text
         FROM spec_open_question
        WHERE spec_id = $1 AND state = 'open'
        ORDER BY section_id, id`,
      [specId],
    );

    return {
      specId,
      checkpointId: row.checkpoint_id,
      docSeq: BigInt(row.doc_seq),
      publishedAt: row.published_at,
      sections: pinnedSections(row.state),
      openQuestions: questions.rows.map((question) => ({
        id: question.id,
        sectionId: question.section_id,
        text: question.text,
      })),
    };
  }
}

/** The sections of a checkpoint's Yjs state, in document order. */
export function pinnedSections(state: Uint8Array): PinnedSection[] {
  const document = new Y.Doc();
  try {
    Y.applyUpdate(document, state);
    return readPinnedSections(proseMirrorDocument(document));
  } finally {
    document.destroy();
  }
}

function readPinnedSections(document: ProseMirrorNode): PinnedSection[] {
  const sections: PinnedSection[] = [];
  document.forEach((node) => {
    if (node.type !== schema.nodes.section) return;
    const id = node.attrs["id"];
    if (typeof id !== "string") return;
    sections.push({ id, title: node.firstChild?.textContent || id });
  });
  return sections;
}

const TICKET_COLUMNS = `id, spec_id, parent_id, ordinal, title, description,
       section_id, depends_on, sync_state, linear_id, sync_error`;

interface TicketRow {
  id: string;
  spec_id: string;
  parent_id: string | null;
  ordinal: number;
  title: string;
  description: string;
  section_id: string;
  depends_on: string[];
  sync_state: SpecTicketNode["syncState"];
  linear_id: string | null;
  sync_error: string | null;
}

function ticketRecord(row: TicketRow): SpecTicketDraftRecord {
  return {
    id: row.id,
    specId: row.spec_id,
    parentId: row.parent_id,
    ordinal: row.ordinal,
    title: row.title,
    description: row.description,
    sectionId: row.section_id,
    dependsOn: row.depends_on,
    syncState: row.sync_state,
    linearId: row.linear_id,
    syncError: row.sync_error,
  };
}

export class PostgresSpecTicketStore implements SpecTicketStore {
  constructor(private readonly pool: Pool) {}

  async list(specId: string): Promise<SpecTicketDraftRecord[]> {
    const result = await this.pool.query<TicketRow>(
      `SELECT ${TICKET_COLUMNS} FROM spec_ticket_draft WHERE spec_id = $1
        ORDER BY ordinal, id`,
      [specId],
    );
    return result.rows.map(ticketRecord);
  }

  async mutate(
    specId: string,
    apply: (current: SpecTicketDraftRecord[]) => SpecTicketDraftRecord[],
  ): Promise<SpecTicketDraftRecord[]> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      // The spec row is the tree's lock, the same one the document store
      // takes. Two drags, or a drag and a proposal, serialise here.
      const locked = await client.query("SELECT id FROM spec WHERE id = $1 FOR UPDATE", [specId]);
      if (locked.rowCount === 0) throw new SpecTicketError("not_found", `Unknown spec: ${specId}`);
      const current = await client.query<TicketRow>(
        `SELECT ${TICKET_COLUMNS} FROM spec_ticket_draft WHERE spec_id = $1 ORDER BY ordinal, id`,
        [specId],
      );
      const next = apply(current.rows.map(ticketRecord));
      await writeTree(client, specId, current.rows.map(ticketRecord), next);
      await client.query("COMMIT");
      return next;
    } catch (error) {
      await client.query("ROLLBACK").catch(() => undefined);
      throw error;
    } finally {
      client.release();
    }
  }
}

/**
 * Write the new tree over the old one.
 *
 * Parents are detached before the deletes, because `parent_id` cascades: a
 * delete of a ticket whose children the caller kept would otherwise take them
 * too. Ordinals go to a disjoint range first, so no intermediate state trips
 * a future uniqueness constraint on `(spec_id, parent_id, ordinal)`.
 */
async function writeTree(
  client: PoolClient,
  specId: string,
  current: readonly SpecTicketDraftRecord[],
  next: readonly SpecTicketDraftRecord[],
): Promise<void> {
  const keep = new Set(next.map((row) => row.id));
  const removed = current.filter((row) => !keep.has(row.id)).map((row) => row.id);

  if (current.length > 0) {
    await client.query(
      `UPDATE spec_ticket_draft SET parent_id = NULL, ordinal = ordinal + $2
        WHERE spec_id = $1`,
      [specId, ORDINAL_PARK],
    );
  }
  if (removed.length > 0) {
    await client.query(`DELETE FROM spec_ticket_draft WHERE spec_id = $1 AND id = ANY($2::uuid[])`, [
      specId,
      removed,
    ]);
  }
  for (const row of next) {
    await client.query(
      `INSERT INTO spec_ticket_draft
         (id, spec_id, parent_id, ordinal, title, description, section_id,
          depends_on, sync_state, linear_id, sync_error)
       VALUES ($1, $2, NULL, $3 + ${ORDINAL_PARK}, $4, $5, $6, $7, $8, $9, $10)
       ON CONFLICT (id) DO UPDATE SET
         ordinal = EXCLUDED.ordinal,
         title = EXCLUDED.title,
         description = EXCLUDED.description,
         section_id = EXCLUDED.section_id,
         depends_on = EXCLUDED.depends_on,
         sync_state = EXCLUDED.sync_state,
         linear_id = EXCLUDED.linear_id,
         sync_error = EXCLUDED.sync_error`,
      [
        row.id,
        specId,
        row.ordinal,
        row.title,
        row.description,
        row.sectionId,
        JSON.stringify(row.dependsOn),
        row.syncState,
        row.linearId,
        row.syncError,
      ],
    );
  }
  // Every row exists now, so the parent links can be restored in one pass.
  for (const row of next) {
    await client.query(`UPDATE spec_ticket_draft SET parent_id = $2, ordinal = $3 WHERE id = $1`, [
      row.id,
      row.parentId,
      row.ordinal,
    ]);
  }
}

/** A range no live ordinal uses, so a rewrite never collides with itself. */
const ORDINAL_PARK = 1_000_000;
