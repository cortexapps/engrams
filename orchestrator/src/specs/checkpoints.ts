import { findSection, replaceSection } from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import type { Pool } from "pg";
import * as Y from "yjs";
import {
  proseMirrorDocument,
  type SpecDocumentService,
  type SpecUpdateRecord,
} from "./doc-service.ts";

export interface CreateCheckpointOptions {
  reason: string;
  label: string;
  authorUserId?: string | null;
}

export interface SpecCheckpointRecord {
  id: string;
  specId: string;
  state: Uint8Array;
  stateVector: Uint8Array;
  renderedMarkdown: string;
  docSeq: bigint;
  label: string;
  authorUserId: string | null;
  reason: string;
  createdAt: Date;
}

export interface SpecCheckpointStore {
  insertCheckpoint(checkpoint: SpecCheckpointRecord): Promise<void>;
  readCheckpoint(specId: string, checkpointId: string): Promise<SpecCheckpointRecord | null>;
}

export interface RestoreSectionResult {
  checkpointBeforeRestore: SpecCheckpointRecord;
  update: SpecUpdateRecord;
}

export class SpecCheckpointService {
  constructor(
    private readonly documents: SpecDocumentService,
    private readonly store: SpecCheckpointStore,
  ) {}

  async createCheckpoint(
    specId: string,
    options: CreateCheckpointOptions,
  ): Promise<SpecCheckpointRecord> {
    const compacted = await this.documents.compact(specId);
    const checkpoint: SpecCheckpointRecord = {
      id: randomUUID(),
      specId,
      state: compacted.state,
      stateVector: compacted.stateVector,
      renderedMarkdown: compacted.renderedMarkdown,
      docSeq: compacted.coveredSeq,
      label: options.label,
      authorUserId: options.authorUserId ?? null,
      reason: options.reason,
      createdAt: new Date(),
    };
    await this.store.insertCheckpoint(checkpoint);
    return checkpoint;
  }

  async restoreSection(
    specId: string,
    checkpointId: string,
    sectionId: string,
    authorUserId: string | null = null,
  ): Promise<RestoreSectionResult> {
    const source = await this.store.readCheckpoint(specId, checkpointId);
    if (!source) throw new Error(`Unknown spec checkpoint: ${checkpointId}`);

    const sourceDoc = new Y.Doc();
    Y.applyUpdate(sourceDoc, source.state);
    const restored = findSection(proseMirrorDocument(sourceDoc), sectionId);
    if (!restored) throw new Error(`Checkpoint does not contain section: ${sectionId}`);

    // Restore is a forward edit. Save the current state first so this restore
    // can itself be restored.
    const checkpointBeforeRestore = await this.createCheckpoint(specId, {
      reason: "before_restore",
      label: `Before restore of ${sectionId}`,
      authorUserId,
    });
    const update = await this.documents.mutateDocument(
      specId,
      `checkpoint:${checkpointId}`,
      (current) => replaceSection(current, sectionId, restored.node),
    );
    return { checkpointBeforeRestore, update };
  }
}

interface CheckpointRow {
  id: string;
  spec_id: string;
  state: Buffer;
  state_vector: Buffer;
  rendered_markdown: string;
  doc_seq: string;
  label: string;
  author_user_id: string | null;
  reason: string;
  created_at: Date;
}

export class PostgresSpecCheckpointStore implements SpecCheckpointStore {
  constructor(private readonly pool: Pool) {}

  async insertCheckpoint(checkpoint: SpecCheckpointRecord): Promise<void> {
    await this.pool.query(
      `INSERT INTO spec_checkpoint
         (id, spec_id, state, state_vector, rendered_markdown, doc_seq,
          label, author_user_id, reason, created_at)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)`,
      [
        checkpoint.id,
        checkpoint.specId,
        Buffer.from(checkpoint.state),
        Buffer.from(checkpoint.stateVector),
        checkpoint.renderedMarkdown,
        checkpoint.docSeq.toString(),
        checkpoint.label,
        checkpoint.authorUserId,
        checkpoint.reason,
        checkpoint.createdAt,
      ],
    );
  }

  async readCheckpoint(
    specId: string,
    checkpointId: string,
  ): Promise<SpecCheckpointRecord | null> {
    const result = await this.pool.query<CheckpointRow>(
      `SELECT id, spec_id, state, state_vector, rendered_markdown, doc_seq,
              label, author_user_id, reason, created_at
         FROM spec_checkpoint
        WHERE spec_id = $1 AND id = $2`,
      [specId, checkpointId],
    );
    const row = result.rows[0];
    return row
      ? {
          id: row.id,
          specId: row.spec_id,
          state: row.state,
          stateVector: row.state_vector,
          renderedMarkdown: row.rendered_markdown,
          docSeq: BigInt(row.doc_seq),
          label: row.label,
          authorUserId: row.author_user_id,
          reason: row.reason,
          createdAt: row.created_at,
        }
      : null;
  }
}
