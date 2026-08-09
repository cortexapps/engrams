import { renderMarkdown, schema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment, yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";
import * as Y from "yjs";
import type { Pool, PoolClient } from "pg";

export const SPEC_UPDATE_CHANNEL = "spec_update";
export const SPEC_SOFT_SIZE_BYTES = 500 * 1024;
export const SPEC_MAX_SIZE_BYTES = 2 * 1024 * 1024;

export interface SpecUpdateChannelEnvelope {
  type: "update";
  specId: string;
  seq: string;
}

export interface SpecAwarenessChannelEnvelope {
  type: "awareness";
  specId: string;
  update: string;
}

export type SpecChannelEnvelope = SpecUpdateChannelEnvelope | SpecAwarenessChannelEnvelope;

export function encodeSpecChannelEnvelope(envelope: SpecChannelEnvelope): string {
  return JSON.stringify(envelope);
}

export function parseSpecChannelEnvelope(payload: string): SpecChannelEnvelope | null {
  try {
    const value: unknown = JSON.parse(payload);
    if (!value || typeof value !== "object" || Array.isArray(value)) return null;
    const record = value as Record<string, unknown>;
    if (typeof record.type !== "string" || typeof record.specId !== "string") return null;
    if (record.type === "update" && typeof record.seq === "string") {
      return { type: "update", specId: record.specId, seq: record.seq };
    }
    if (record.type === "awareness" && typeof record.update === "string") {
      return { type: "awareness", specId: record.specId, update: record.update };
    }
    return null;
  } catch {
    return null;
  }
}

export class SpecDocumentTooLargeError extends Error {
  constructor(readonly renderedSizeBytes: number) {
    super(`The spec document is ${renderedSizeBytes} bytes; the limit is ${SPEC_MAX_SIZE_BYTES}`);
    this.name = "SpecDocumentTooLargeError";
  }
}

export interface SpecSnapshotRecord {
  state: Uint8Array;
  stateVector: Uint8Array;
  coveredSeq: bigint;
}

export interface SpecUpdateRecord {
  seq: bigint;
  update: Uint8Array;
  clientId: string | null;
}

export interface CompactSnapshotInput extends SpecSnapshotRecord {
  specId: string;
}

export interface SpecDocumentStore {
  readSnapshot(specId: string): Promise<SpecSnapshotRecord | null>;
  readUpdatesAfter(specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]>;
  insertUpdate(specId: string, update: Uint8Array, clientId: string | null): Promise<bigint>;
  notifyUpdate(specId: string, seq: bigint): Promise<void>;
  compactSnapshot(input: CompactSnapshotInput): Promise<boolean>;
  listen(onWake: (specId: string) => void): Promise<() => Promise<void>>;
}

export interface LoadedSpecDocument {
  readonly doc: Y.Doc;
  readonly lastAppliedSeq: bigint;
}

interface CachedSpecDocument {
  doc: Y.Doc;
  lastAppliedSeq: bigint;
}

export interface SpecDocumentUpdateEvent {
  specId: string;
  seq: bigint;
  update: Uint8Array;
  clientId: string | null;
  source: "local" | "peer";
}

export interface SpecDocumentServiceOptions {
  onWarning?: (message: string) => void;
  /** Test seam for a process failure after the durable insert. */
  afterPersist?: (specId: string, seq: bigint) => void | Promise<void>;
}

export interface CompactSpecResult extends SpecSnapshotRecord {
  renderedMarkdown: string;
}

export function proseMirrorDocument(doc: Y.Doc): ProseMirrorNode {
  const fragment = doc.getXmlFragment(SPEC_FRAGMENT_NAME);
  if (fragment.length === 0) throw new Error("The spec document has no sections");
  return yXmlFragmentToProseMirrorRootNode(fragment, schema);
}

export function encodeProseMirrorDocument(doc: ProseMirrorNode): Uint8Array {
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(doc, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  return Y.encodeStateAsUpdate(ydoc);
}

export class SpecDocumentService {
  private readonly cache = new Map<string, CachedSpecDocument>();
  private readonly locks = new Map<string, Promise<void>>();
  private readonly listeners = new Map<string, Set<(event: SpecDocumentUpdateEvent) => void>>();
  private stopListening: (() => Promise<void>) | null = null;

  constructor(
    private readonly store: SpecDocumentStore,
    private readonly options: SpecDocumentServiceOptions = {},
  ) {}

  async startPeerSync(): Promise<void> {
    if (this.stopListening) return;
    this.stopListening = await this.store.listen((specId) => {
      void this.syncFromLog(specId).catch((error: unknown) => {
        this.warn(`Spec update sync failed for ${specId}: ${errorMessage(error)}`);
      });
    });
  }

  async stopPeerSync(): Promise<void> {
    const stop = this.stopListening;
    this.stopListening = null;
    if (stop) await stop();
  }

  async loadDoc(specId: string): Promise<LoadedSpecDocument> {
    return this.withLock(specId, () => this.loadUnlocked(specId));
  }

  async syncFromLog(specId: string): Promise<LoadedSpecDocument> {
    return this.withLock(specId, async () => {
      const room = await this.loadUnlocked(specId);
      await this.syncUnlocked(specId, room);
      return room;
    });
  }

  async applyUpdate(
    specId: string,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<SpecUpdateRecord> {
    return this.withLock(specId, async () => {
      const room = await this.loadUnlocked(specId);
      await this.syncUnlocked(specId, room);
      return this.applyUpdateUnlocked(specId, room, update, clientId);
    });
  }

  async mutateDocument(
    specId: string,
    clientId: string | null,
    mutate: (doc: ProseMirrorNode) => ProseMirrorNode,
  ): Promise<SpecUpdateRecord> {
    return this.withLock(specId, async () => {
      const room = await this.loadUnlocked(specId);
      await this.syncUnlocked(specId, room);

      const fork = new Y.Doc();
      Y.applyUpdate(fork, Y.encodeStateAsUpdate(room.doc));
      const before = Y.encodeStateVector(fork);
      const replacement = mutate(proseMirrorDocument(fork));
      prosemirrorToYXmlFragment(replacement, fork.getXmlFragment(SPEC_FRAGMENT_NAME));
      const update = Y.encodeStateAsUpdate(fork, before);
      if (update.length === 2) throw new Error("The spec mutation did not change the document");
      return this.applyUpdateUnlocked(specId, room, update, clientId);
    });
  }

  subscribe(
    specId: string,
    listener: (event: SpecDocumentUpdateEvent) => void,
  ): () => void {
    const listeners = this.listeners.get(specId) ?? new Set();
    listeners.add(listener);
    this.listeners.set(specId, listeners);
    return () => {
      listeners.delete(listener);
      if (listeners.size === 0) this.listeners.delete(specId);
    };
  }

  async compact(specId: string): Promise<CompactSpecResult> {
    return this.withLock(specId, async () => {
      const room = await this.loadUnlocked(specId);
      await this.syncUnlocked(specId, room);
      const state = Y.encodeStateAsUpdate(room.doc);
      const stateVector = Y.encodeStateVector(room.doc);
      const renderedMarkdown = renderMarkdown(proseMirrorDocument(room.doc));
      const input = {
        specId,
        state,
        stateVector,
        coveredSeq: room.lastAppliedSeq,
      };
      await this.store.compactSnapshot(input);
      return { state, stateVector, coveredSeq: room.lastAppliedSeq, renderedMarkdown };
    });
  }

  evict(specId: string): void {
    this.cache.get(specId)?.doc.destroy();
    this.cache.delete(specId);
  }

  private async loadUnlocked(specId: string): Promise<CachedSpecDocument> {
    const cached = this.cache.get(specId);
    if (cached) return cached;

    const doc = new Y.Doc();
    const snapshot = await this.store.readSnapshot(specId);
    let lastAppliedSeq = 0n;
    if (snapshot) {
      Y.applyUpdate(doc, snapshot.state);
      lastAppliedSeq = snapshot.coveredSeq;
    }
    const room = { doc, lastAppliedSeq };
    await this.applyTail(specId, room, "peer");
    this.cache.set(specId, room);
    return room;
  }

  private async syncUnlocked(specId: string, room: CachedSpecDocument): Promise<void> {
    const snapshot = await this.store.readSnapshot(specId);
    if (snapshot && snapshot.coveredSeq > room.lastAppliedSeq) {
      Y.applyUpdate(room.doc, snapshot.state);
      room.lastAppliedSeq = snapshot.coveredSeq;
    }
    await this.applyTail(specId, room, "peer");
  }

  private async applyTail(
    specId: string,
    room: CachedSpecDocument,
    source: "peer",
  ): Promise<void> {
    const updates = await this.store.readUpdatesAfter(specId, room.lastAppliedSeq);
    for (const row of updates) {
      Y.applyUpdate(room.doc, row.update);
      room.lastAppliedSeq = row.seq;
      this.broadcast({ specId, ...row, source });
    }
  }

  private async applyUpdateUnlocked(
    specId: string,
    room: CachedSpecDocument,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<SpecUpdateRecord> {
    this.validateSize(room.doc, update);

    // ADR 0114 D5: the durable insert must complete before any local state or
    // socket can observe the update.
    const seq = await this.store.insertUpdate(specId, update, clientId);
    await this.options.afterPersist?.(specId, seq);
    Y.applyUpdate(room.doc, update);
    room.lastAppliedSeq = seq;
    const row = { seq, update, clientId };
    this.broadcast({ specId, ...row, source: "local" });
    await this.store.notifyUpdate(specId, seq);
    return row;
  }

  private validateSize(current: Y.Doc, update: Uint8Array): void {
    const candidate = new Y.Doc();
    Y.applyUpdate(candidate, Y.encodeStateAsUpdate(current));
    Y.applyUpdate(candidate, update);
    const markdown = renderMarkdown(proseMirrorDocument(candidate));
    const size = new TextEncoder().encode(markdown).byteLength;
    if (size > SPEC_MAX_SIZE_BYTES) throw new SpecDocumentTooLargeError(size);
    if (size > SPEC_SOFT_SIZE_BYTES) {
      this.warn(`Spec document is ${size} bytes; the soft limit is ${SPEC_SOFT_SIZE_BYTES}`);
    }
    candidate.destroy();
  }

  private broadcast(event: SpecDocumentUpdateEvent): void {
    for (const listener of this.listeners.get(event.specId) ?? []) {
      try {
        listener(event);
      } catch (error: unknown) {
        this.warn(`Spec update listener failed for ${event.specId}: ${errorMessage(error)}`);
      }
    }
  }

  private warn(message: string): void {
    this.options.onWarning?.(message);
  }

  private async withLock<T>(specId: string, action: () => Promise<T>): Promise<T> {
    const previous = this.locks.get(specId) ?? Promise.resolve();
    let release = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const queued = previous.then(() => gate);
    this.locks.set(specId, queued);
    await previous;
    try {
      return await action();
    } finally {
      release();
      if (this.locks.get(specId) === queued) this.locks.delete(specId);
    }
  }
}

interface UpdateRow {
  seq: string;
  update: Buffer;
  client_id: string | null;
}

interface SnapshotRow {
  state: Buffer;
  state_vector: Buffer;
  covered_seq: string;
}

export class PostgresSpecDocumentStore implements SpecDocumentStore {
  constructor(private readonly pool: Pool) {}

  async readSnapshot(specId: string): Promise<SpecSnapshotRecord | null> {
    const result = await this.pool.query<SnapshotRow>(
      `SELECT state, state_vector, covered_seq
         FROM spec_snapshot
        WHERE spec_id = $1`,
      [specId],
    );
    const row = result.rows[0];
    return row
      ? { state: row.state, stateVector: row.state_vector, coveredSeq: BigInt(row.covered_seq) }
      : null;
  }

  async readUpdatesAfter(specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]> {
    const result = await this.pool.query<UpdateRow>(
      `SELECT seq, update, client_id
         FROM spec_update_log
        WHERE spec_id = $1 AND seq > $2
        ORDER BY seq`,
      [specId, afterSeq.toString()],
    );
    return result.rows.map((row) => ({
      seq: BigInt(row.seq),
      update: row.update,
      clientId: row.client_id,
    }));
  }

  async insertUpdate(
    specId: string,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<bigint> {
    const result = await this.pool.query<{ seq: string }>(
      `INSERT INTO spec_update_log (spec_id, update, client_id)
       VALUES ($1, $2, $3)
       RETURNING seq`,
      [specId, Buffer.from(update), clientId],
    );
    return BigInt(result.rows[0]!.seq);
  }

  async notifyUpdate(specId: string, seq: bigint): Promise<void> {
    await this.pool.query("SELECT pg_notify($1, $2)", [
      SPEC_UPDATE_CHANNEL,
      encodeSpecChannelEnvelope({ type: "update", specId, seq: seq.toString() }),
    ]);
  }

  async compactSnapshot(input: CompactSnapshotInput): Promise<boolean> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const lock = await client.query<{ acquired: boolean }>(
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0)) AS acquired",
        [`spec-compact:${input.specId}`],
      );
      if (!lock.rows[0]?.acquired) {
        await client.query("ROLLBACK");
        return false;
      }
      await client.query(
        `INSERT INTO spec_snapshot
           (spec_id, state, state_vector, covered_seq, created_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (spec_id) DO UPDATE
         SET state = excluded.state,
             state_vector = excluded.state_vector,
             covered_seq = excluded.covered_seq,
             created_at = excluded.created_at
         WHERE spec_snapshot.covered_seq <= excluded.covered_seq`,
        [
          input.specId,
          Buffer.from(input.state),
          Buffer.from(input.stateVector),
          input.coveredSeq.toString(),
        ],
      );
      await client.query("DELETE FROM spec_update_log WHERE spec_id = $1 AND seq <= $2", [
        input.specId,
        input.coveredSeq.toString(),
      ]);
      await client.query("COMMIT");
      return true;
    } catch (error) {
      await rollback(client);
      throw error;
    } finally {
      client.release();
    }
  }

  async listen(onWake: (specId: string) => void): Promise<() => Promise<void>> {
    const client = await this.pool.connect();
    const onNotification = (message: { channel: string; payload?: string }) => {
      if (message.channel !== SPEC_UPDATE_CHANNEL || !message.payload) return;
      const envelope = parseSpecChannelEnvelope(message.payload);
      if (envelope?.type === "update") onWake(envelope.specId);
    };
    client.on("notification", onNotification);
    await client.query(`LISTEN ${SPEC_UPDATE_CHANNEL}`);
    return async () => {
      client.off("notification", onNotification);
      await client.query(`UNLISTEN ${SPEC_UPDATE_CHANNEL}`).catch(() => {});
      client.release();
    };
  }
}

async function rollback(client: PoolClient): Promise<void> {
  await client.query("ROLLBACK").catch(() => {});
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
