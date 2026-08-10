import {
  renderMarkdown,
  schema,
  SPEC_FRAGMENT_NAME,
  validateRequirementEdit,
} from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment, yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";
import * as Y from "yjs";
import type { Pool, PoolClient } from "pg";

import { applyHumanSectionEdit, type SectionStateValue } from "./section-state.ts";
import { humanEditRequestFingerprint } from "./section-state-service.ts";

export const SPEC_UPDATE_CHANNEL = "spec_update";
export const SPEC_SOFT_SIZE_BYTES = 500 * 1024;
export const SPEC_MAX_SIZE_BYTES = 2 * 1024 * 1024;
export const SPEC_UPDATE_SIZE_FACTOR = 16;

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

export class SpecDocumentRevisionConflictError extends Error {
  constructor(
    readonly expectedSeq: bigint,
    readonly actualSeq: bigint,
  ) {
    super(`The spec document is at revision ${actualSeq}; expected ${expectedSeq}`);
    this.name = "SpecDocumentRevisionConflictError";
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

export interface SpecDocumentSectionEffect {
  id: string;
  title: string;
  changed: boolean;
}

export interface SpecUpdateEffects {
  sections: readonly SpecDocumentSectionEffect[];
  at?: Date;
}

export interface CompactSnapshotInput extends SpecSnapshotRecord {
  specId: string;
}

export interface SpecDocumentStore {
  readSnapshot(specId: string): Promise<SpecSnapshotRecord | null>;
  readUpdatesAfter(specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]>;
  insertUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<bigint | null>;
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
  validationDoc: Y.Doc;
  lastAppliedSeq: bigint;
  renderedSizeUpperBound: number;
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
  measureRenderedSize?: (doc: ProseMirrorNode) => number;
  /** Injected wall time for durable human-edit actions. */
  now?: () => Date;
}

export interface CompactSpecResult extends SpecSnapshotRecord {
  renderedMarkdown: string;
}

export class SpecCompactionStaleError extends Error {
  constructor(readonly specId: string) {
    super(`Spec ${specId} changed during compaction; retry it later`);
    this.name = "SpecCompactionStaleError";
  }
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
      if (!this.cache.has(specId)) return;
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
    mutate: (doc: ProseMirrorNode, ydoc: Y.Doc) => ProseMirrorNode,
    expectedSeq?: bigint,
  ): Promise<SpecUpdateRecord> {
    return this.withLock(specId, async () => {
      const room = await this.loadUnlocked(specId);
      for (;;) {
        await this.syncUnlocked(specId, room);
        if (expectedSeq !== undefined && room.lastAppliedSeq !== expectedSeq) {
          throw new SpecDocumentRevisionConflictError(expectedSeq, room.lastAppliedSeq);
        }
        const fork = new Y.Doc();
        try {
          Y.applyUpdate(fork, Y.encodeStateAsUpdate(room.doc));
          const before = Y.encodeStateVector(fork);
          const replacement = mutate(proseMirrorDocument(fork), fork);
          prosemirrorToYXmlFragment(replacement, fork.getXmlFragment(SPEC_FRAGMENT_NAME));
          const update = Y.encodeStateAsUpdate(fork, before);
          if (update.length === 2) throw new Error("The spec mutation did not change the document");
          const stored = await this.tryApplyUpdateUnlocked(specId, room, update, clientId);
          if (stored) return stored;
        } finally {
          fork.destroy();
        }
      }
    });
  }

  subscribe(specId: string, listener: (event: SpecDocumentUpdateEvent) => void): () => void {
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
      for (let attempt = 0; attempt < 4; attempt += 1) {
        await this.syncUnlocked(specId, room);
        const state = Y.encodeStateAsUpdate(room.doc);
        const stateVector = Y.encodeStateVector(room.doc);
        const renderedMarkdown = renderMarkdown(proseMirrorDocument(room.doc));
        const coveredSeq = room.lastAppliedSeq;
        const input = { specId, state, stateVector, coveredSeq };
        if (await this.store.compactSnapshot(input)) {
          return { state, stateVector, coveredSeq, renderedMarkdown };
        }
      }
      throw new SpecCompactionStaleError(specId);
    });
  }

  evict(specId: string): void {
    const room = this.cache.get(specId);
    room?.doc.destroy();
    room?.validationDoc.destroy();
    this.cache.delete(specId);
  }

  private async loadUnlocked(specId: string): Promise<CachedSpecDocument> {
    const cached = this.cache.get(specId);
    if (cached) return cached;

    const doc = new Y.Doc();
    const validationDoc = new Y.Doc();
    const snapshot = await this.store.readSnapshot(specId);
    let lastAppliedSeq = 0n;
    if (snapshot) {
      Y.applyUpdate(doc, snapshot.state);
      Y.applyUpdate(validationDoc, snapshot.state);
      lastAppliedSeq = snapshot.coveredSeq;
    }
    const room = { doc, validationDoc, lastAppliedSeq, renderedSizeUpperBound: 0 };
    await this.applyTail(specId, room, "peer");
    if (doc.getXmlFragment(SPEC_FRAGMENT_NAME).length > 0) {
      room.renderedSizeUpperBound = this.measureRenderedSize(proseMirrorDocument(doc));
    }
    this.cache.set(specId, room);
    return room;
  }

  private async syncUnlocked(specId: string, room: CachedSpecDocument): Promise<void> {
    const snapshot = await this.store.readSnapshot(specId);
    if (snapshot && snapshot.coveredSeq > room.lastAppliedSeq) {
      Y.applyUpdate(room.doc, snapshot.state);
      Y.applyUpdate(room.validationDoc, snapshot.state);
      room.lastAppliedSeq = snapshot.coveredSeq;
    }
    await this.applyTail(specId, room, "peer");
  }

  private async applyTail(specId: string, room: CachedSpecDocument, source: "peer"): Promise<void> {
    const updates = await this.store.readUpdatesAfter(specId, room.lastAppliedSeq);
    for (const row of updates) {
      Y.applyUpdate(room.doc, row.update);
      Y.applyUpdate(room.validationDoc, row.update);
      room.lastAppliedSeq = row.seq;
      room.renderedSizeUpperBound += estimatedRenderedGrowth(row.update);
      this.broadcast({ specId, ...row, source });
    }
    if (room.renderedSizeUpperBound >= SPEC_SOFT_SIZE_BYTES) {
      room.renderedSizeUpperBound = this.measureRenderedSize(proseMirrorDocument(room.doc));
    }
  }

  private async applyUpdateUnlocked(
    specId: string,
    room: CachedSpecDocument,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<SpecUpdateRecord> {
    // Decode the untrusted client payload before it can enter the durable log.
    // validateCandidate applies it to the private shadow and enforces the full
    // ProseMirror document schema before persistence.
    Y.decodeUpdate(update);

    for (;;) {
      const stored = await this.tryApplyUpdateUnlocked(specId, room, update, clientId);
      if (stored) return stored;
      await this.syncUnlocked(specId, room);
    }
  }

  private async tryApplyUpdateUnlocked(
    specId: string,
    room: CachedSpecDocument,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<SpecUpdateRecord | null> {
    const { candidate, sections } = this.validateCandidate(room, update);
    const renderedSizeUpperBound = this.validateSize(room, update, candidate);
    let seq: bigint | null;
    try {
      seq = await this.store.insertUpdateIfLatest(specId, room.lastAppliedSeq, update, clientId, {
        sections,
        at:
          clientId !== null && sections.some((section) => section.changed)
            ? this.options.now?.()
            : undefined,
      });
    } catch (error) {
      this.resetValidationDoc(room);
      throw error;
    }
    if (seq === null) {
      this.resetValidationDoc(room);
      return null;
    }

    try {
      await this.options.afterPersist?.(specId, seq);
    } catch (error) {
      this.resetValidationDoc(room);
      throw error;
    }
    Y.applyUpdate(room.doc, update);
    room.lastAppliedSeq = seq;
    room.renderedSizeUpperBound = renderedSizeUpperBound;
    const row = { seq, update, clientId };
    this.broadcast({ specId, ...row, source: "local" });
    await this.store.notifyUpdate(specId, seq);
    return row;
  }

  private validateCandidate(
    room: CachedSpecDocument,
    update: Uint8Array,
  ): { candidate: ProseMirrorNode; sections: readonly SpecDocumentSectionEffect[] } {
    const validationDoc = room.validationDoc;
    Y.applyUpdate(validationDoc, update);
    let repaired = false;
    const onRepair = () => {
      repaired = true;
    };
    validationDoc.on("update", onRepair);
    try {
      const candidate = proseMirrorDocument(validationDoc);
      candidate.check();
      if (repaired) throw new Error("The spec update violates the document schema");
      const before =
        room.doc.getXmlFragment(SPEC_FRAGMENT_NAME).length === 0
          ? null
          : proseMirrorDocument(room.doc);
      const sections = compareSections(before, candidate);
      validateRequirementsSection(before, candidate, sections);
      return { candidate, sections };
    } catch (error) {
      this.resetValidationDoc(room);
      throw error;
    } finally {
      validationDoc.off("update", onRepair);
    }
  }

  private validateSize(
    room: CachedSpecDocument,
    update: Uint8Array,
    candidate: ProseMirrorNode,
  ): number {
    const upperBound = room.renderedSizeUpperBound + estimatedRenderedGrowth(update);
    if (
      upperBound < SPEC_SOFT_SIZE_BYTES &&
      room.doc.getXmlFragment(SPEC_FRAGMENT_NAME).length > 0
    ) {
      return upperBound;
    }

    try {
      const size = this.measureRenderedSize(candidate);
      if (size > SPEC_MAX_SIZE_BYTES) throw new SpecDocumentTooLargeError(size);
      if (size > SPEC_SOFT_SIZE_BYTES) {
        this.warn(`Spec document is ${size} bytes; the soft limit is ${SPEC_SOFT_SIZE_BYTES}`);
      }
      return size;
    } catch (error) {
      this.resetValidationDoc(room);
      throw error;
    }
  }

  private resetValidationDoc(room: CachedSpecDocument): void {
    room.validationDoc.destroy();
    room.validationDoc = new Y.Doc();
    Y.applyUpdate(room.validationDoc, Y.encodeStateAsUpdate(room.doc));
  }

  private measureRenderedSize(doc: ProseMirrorNode): number {
    return (
      this.options.measureRenderedSize ??
      ((value) => new TextEncoder().encode(renderMarkdown(value)).byteLength)
    )(doc);
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

function compareSections(
  before: ProseMirrorNode | null,
  candidate: ProseMirrorNode,
): readonly SpecDocumentSectionEffect[] {
  const prior = before ? documentSections(before) : [];
  const next = documentSections(candidate);
  if (
    prior.length > 0 &&
    (prior.length !== next.length || prior.some((section, index) => section.id !== next[index]?.id))
  ) {
    throw new Error("A spec update cannot add, remove, reorder or replace template sections.");
  }
  return next.map((section, index) => ({
    id: section.id,
    title: section.title,
    changed: prior[index] == null || !prior[index].node.eq(section.node),
  }));
}

interface DocumentSection {
  id: string;
  key: string;
  title: string;
  node: ProseMirrorNode;
}

function documentSections(doc: ProseMirrorNode): DocumentSection[] {
  const sections: DocumentSection[] = [];
  doc.forEach((node) => {
    if (node.type !== schema.nodes.section) return;
    const id = node.attrs.id;
    const key = node.attrs.templateSectionKey;
    if (typeof id !== "string" || id.length === 0 || typeof key !== "string" || key.length === 0) {
      throw new Error("Every spec section must keep its template identity.");
    }
    sections.push({ id, key, title: node.firstChild?.textContent ?? id, node });
  });
  return sections;
}

function validateRequirementsSection(
  before: ProseMirrorNode | null,
  candidate: ProseMirrorNode,
  effects: readonly SpecDocumentSectionEffect[],
): void {
  const prior = before
    ? documentSections(before).find((section) => section.key === "requirements")
    : null;
  const next = documentSections(candidate).find((section) => section.key === "requirements");
  if (!next || !effects.find((effect) => effect.id === next.id)?.changed) return;
  validateRequirementEdit(prior?.node ?? "", next.node);
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

  async insertUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<bigint | null> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const revision = await client.query<{ current_doc_seq: string }>(
        `UPDATE spec
            SET current_doc_seq = current_doc_seq + 1
          WHERE id = $1
            AND current_doc_seq = $2
        RETURNING current_doc_seq`,
        [specId, expectedSeq.toString()],
      );
      const nextSeq = revision.rows[0]?.current_doc_seq;
      if (nextSeq === undefined) {
        await client.query("ROLLBACK");
        return null;
      }
      await client.query(
        `INSERT INTO spec_update_log (spec_id, seq, update, client_id)
         VALUES ($1, $2, $3, $4)`,
        [specId, nextSeq, Buffer.from(update), clientId],
      );
      if (clientId !== null && effects.sections.some((section) => section.changed)) {
        const participant = await client.query<{ user_id: string | null }>(
          `SELECT user_id
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, clientId],
        );
        const actorUserId = participant.rows[0]?.user_id;
        if (actorUserId) {
          if (!effects.at) throw new Error("A human spec update requires an injected timestamp.");
          await draftHumanEditedSections(client, specId, BigInt(nextSeq), actorUserId, {
            ...effects,
            at: effects.at,
          });
        }
      }
      await client.query("COMMIT");
      return BigInt(nextSeq);
    } catch (error) {
      await rollback(client);
      throw error;
    } finally {
      client.release();
    }
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
      const revision = await client.query<{ current_doc_seq: string }>(
        `SELECT current_doc_seq
           FROM spec
          WHERE id = $1
          FOR UPDATE`,
        [input.specId],
      );
      const currentSeq = revision.rows[0]?.current_doc_seq;
      if (currentSeq === undefined) throw new Error(`Unknown spec: ${input.specId}`);
      if (BigInt(currentSeq) !== input.coveredSeq) {
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

interface HumanSectionStateRow {
  section_id: string;
  state: "empty" | "drafted" | "confirmed" | "n/a";
  na_reason: string | null;
}

async function draftHumanEditedSections(
  client: PoolClient,
  specId: string,
  seq: bigint,
  actorUserId: string,
  effects: SpecUpdateEffects & { at: Date },
): Promise<void> {
  const rows = await client.query<HumanSectionStateRow>(
    `SELECT section_id, state, na_reason
       FROM spec_section_state
      WHERE spec_id = $1`,
    [specId],
  );
  const states = new Map<string, SectionStateValue>(
    rows.rows.map((row) => [row.section_id, { state: row.state, naReason: row.na_reason }]),
  );
  const priorSeq = seq - 1n;

  for (const [index, section] of effects.sections.entries()) {
    if (!section.changed) continue;
    const current = states.get(section.id) ?? { state: "empty", naReason: null };
    const unconfirmedUpstreamSectionIds = effects.sections
      .slice(0, index)
      .filter((upstream) => {
        const state = states.get(upstream.id)?.state ?? "empty";
        return state !== "confirmed" && state !== "n/a";
      })
      .map((upstream) => upstream.id);
    const change = applyHumanSectionEdit(current, {
      specId,
      sectionId: section.id,
      sectionTitle: section.title,
      allowsNa: true,
      unconfirmedUpstreamSectionIds,
    });
    if (!change) continue;

    await client.query(
      `INSERT INTO spec_section_state
         (spec_id, section_id, state, na_reason, confirmed_by, updated_at)
       VALUES ($1, $2, 'drafted', NULL, NULL, $3)
       ON CONFLICT (spec_id, section_id) DO UPDATE
       SET state = 'drafted',
           na_reason = NULL,
           confirmed_by = NULL,
           updated_at = excluded.updated_at`,
      [specId, section.id, effects.at],
    );
    const actionId = `human-edit:${specId}:${seq}:${section.id}`;
    await client.query(
      `INSERT INTO spec_transcript_action
         (id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at)
       VALUES ($1, $2, $3, $4, $5, $6, NULL)`,
      [
        actionId,
        specId,
        section.id,
        humanEditRequestFingerprint(actorUserId, priorSeq),
        change.transcriptChip,
        effects.at,
      ],
    );
    states.set(section.id, change.value);
  }
}

async function rollback(client: PoolClient): Promise<void> {
  await client.query("ROLLBACK").catch(() => {});
}

function estimatedRenderedGrowth(update: Uint8Array): number {
  // Every rendered byte added by the current schema comes from text, a node
  // name, or an attribute value carried in the Yjs update. Markdown escaping
  // can add at most one byte per ASCII source byte. Headings, blank lines, and
  // fences add fewer bytes than their encoded Yjs structure. A factor of 16 is
  // therefore a conservative upper bound for all current nodes. When this
  // bound reaches the 500 KiB soft limit, the service measures the exact render
  // and resets the bound. New rendered node types must extend the bound test.
  return update.byteLength * SPEC_UPDATE_SIZE_FACTOR;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
