import { afterAll, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import {
  createTemplateDocument,
  findSection,
  parseMarkdown,
  replaceSection,
  renderMarkdown,
  RequirementIntegrityError,
  schema,
  SPEC_BLOCK_CACHE_MAX_BYTES,
  SPEC_BLOCK_RENDERER_REVISION,
  SPEC_FRAGMENT_NAME,
  type SpecTemplate,
} from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { Transform } from "prosemirror-transform";
import * as Y from "yjs";
import * as dbSchema from "../../db/schema.ts";
import { makeSpecListStore } from "../../db/specs.ts";
import { PostgresSpecReadStore } from "../../routes/specs.ts";
import {
  encodeProseMirrorDocument,
  encodeSpecChannelEnvelope,
  parseSpecChannelEnvelope,
  PostgresSpecDocumentStore,
  proseMirrorDocument,
  SpecBlockCacheTooLargeError,
  SpecDocumentService,
  SpecDocumentReadOnlyError,
  SpecDocumentRevisionConflictError,
  SpecDocumentTooLargeError,
  SpecParticipantLeaseStaleError,
  SPEC_BLOCK_EDIT_CHECKPOINT_LIMIT,
  SPEC_MAX_SIZE_BYTES,
  SPEC_UPDATE_SIZE_FACTOR,
  type CompactSnapshotInput,
  type SpecDocumentCheckpoint,
  type SpecDocumentStore,
  type SpecSnapshotRecord,
  type SpecUpdateInsertResult,
  type SpecUpdateRecord,
  type SpecUpdateEffects,
} from "../doc-service.ts";
import {
  PostgresSpecCheckpointStore,
  SpecCheckpointService,
  type SpecCheckpointRecord,
  type SpecCheckpointStore,
} from "../checkpoints.ts";
import { PostgresSectionStateStore } from "../section-state-service.ts";
import { transitionSectionState } from "../section-state.ts";
import { PostgresSpecProjectionStore } from "../projection.ts";

const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "requirements", key: "requirements", title: "Requirements" },
    { id: "design", key: "design", title: "Design" },
  ],
};

const SPEC_ID = "00000000-0000-4000-8000-000000000108";

class MemoryDocumentStore implements SpecDocumentStore {
  private readonly currentSeq = new Map<string, bigint>();
  private readonly currentSemanticSeq = new Map<string, bigint>();
  readonly updates = new Map<string, SpecUpdateRecord[]>();
  readonly snapshots = new Map<string, SpecSnapshotRecord>();
  readonly wakes = new Set<(specId: string) => void>();
  readonly reconnects = new Set<() => void>();
  readonly tailReads: string[] = [];
  dropNotifications = false;
  compactions = 0;
  lastEffects: SpecUpdateEffects | null = null;
  draft = true;
  checkpointSink: Map<string, SpecCheckpointRecord> | null = null;

  seedUpdate(specId: string, update: Uint8Array, semanticDocSeq = 1n): void {
    this.currentSeq.set(specId, 1n);
    this.currentSemanticSeq.set(specId, semanticDocSeq);
    this.updates.set(specId, [
      { seq: 1n, semanticDocSeq, update: update.slice(), clientId: "legacy-seed" },
    ]);
  }

  async readSnapshot(specId: string): Promise<SpecSnapshotRecord | null> {
    return this.snapshots.get(specId) ?? null;
  }

  async readUpdatesAfter(specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]> {
    this.tailReads.push(specId);
    return (this.updates.get(specId) ?? []).filter((row) => row.seq > afterSeq);
  }

  async insertUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<SpecUpdateInsertResult | null> {
    if (!this.draft) throw new SpecDocumentReadOnlyError(specId);
    if ((this.currentSeq.get(specId) ?? 0n) !== expectedSeq) return null;
    this.lastEffects = effects;
    const seq = (this.currentSeq.get(specId) ?? 0n) + 1n;
    const semanticDocSeq =
      (this.currentSemanticSeq.get(specId) ?? 0n) + (effects.semanticChanged ? 1n : 0n);
    this.currentSeq.set(specId, seq);
    this.currentSemanticSeq.set(specId, semanticDocSeq);
    const rows = this.updates.get(specId) ?? [];
    rows.push({ seq, semanticDocSeq, update: update.slice(), clientId });
    this.updates.set(specId, rows);
    return { seq, semanticDocSeq };
  }

  async insertCheckpointAndUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    checkpoint: SpecDocumentCheckpoint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<SpecUpdateInsertResult | null> {
    if (!this.draft) throw new SpecDocumentReadOnlyError(specId);
    if ((this.currentSeq.get(specId) ?? 0n) !== expectedSeq) return null;
    this.lastEffects = effects;
    const seq = expectedSeq + 1n;
    const semanticDocSeq =
      (this.currentSemanticSeq.get(specId) ?? 0n) + (effects.semanticChanged ? 1n : 0n);
    this.currentSeq.set(specId, seq);
    this.currentSemanticSeq.set(specId, semanticDocSeq);
    const rows = this.updates.get(specId) ?? [];
    rows.push({ seq, semanticDocSeq, update: update.slice(), clientId });
    this.updates.set(specId, rows);
    this.checkpointSink?.set(checkpoint.id, checkpoint);
    return { seq, semanticDocSeq };
  }

  async notifyUpdate(specId: string): Promise<void> {
    if (!this.dropNotifications) {
      for (const wake of this.wakes) wake(specId);
    }
  }

  async compactSnapshot(input: CompactSnapshotInput): Promise<boolean> {
    this.compactions += 1;
    if ((this.currentSeq.get(input.specId) ?? 0n) !== input.coveredSeq) return false;
    const current = this.snapshots.get(input.specId);
    if (!current || current.coveredSeq <= input.coveredSeq) {
      this.snapshots.set(input.specId, {
        state: input.state.slice(),
        stateVector: input.stateVector.slice(),
        coveredSeq: input.coveredSeq,
        coveredSemanticDocSeq: input.coveredSemanticDocSeq,
      });
    }
    this.updates.set(
      input.specId,
      (this.updates.get(input.specId) ?? []).filter((row) => row.seq > input.coveredSeq),
    );
    return true;
  }

  async listen(
    onWake: (specId: string) => void,
    onReconnect?: () => void,
  ): Promise<() => Promise<void>> {
    this.wakes.add(onWake);
    if (onReconnect) this.reconnects.add(onReconnect);
    return async () => {
      this.wakes.delete(onWake);
      if (onReconnect) this.reconnects.delete(onReconnect);
    };
  }

  reconnect(): void {
    for (const reconnect of this.reconnects) reconnect();
  }
}

class MemoryCheckpointStore implements SpecCheckpointStore {
  readonly checkpoints = new Map<string, SpecCheckpointRecord>();

  async insertCheckpoint(checkpoint: SpecCheckpointRecord): Promise<void> {
    this.checkpoints.set(checkpoint.id, checkpoint);
  }

  async readCheckpoint(specId: string, checkpointId: string): Promise<SpecCheckpointRecord | null> {
    const checkpoint = this.checkpoints.get(checkpointId);
    return checkpoint?.specId === specId ? checkpoint : null;
  }
}

function initialUpdate(): Uint8Array {
  return encodeProseMirrorDocument(createTemplateDocument(TEMPLATE));
}

function diagramUpdate(blockCount = 1): Uint8Array {
  const template = createTemplateDocument(TEMPLATE);
  const sections = template.content.content.map((section) => {
    if (section.attrs.id !== "design") return section;
    const heading = section.firstChild;
    if (!heading) throw new Error("The design test section has no heading");
    const blocks = Array.from({ length: blockCount }, (_, index) =>
      schema.nodes.diagramBlock!.create({
        id: `diagram-${index}`,
        kind: "mermaid",
        source: `flowchart LR\n  A${index} --> B${index}`,
      }),
    );
    return section.type.create(section.attrs, [heading, ...blocks]);
  });
  return encodeProseMirrorDocument(schema.nodes.doc!.create(null, sections));
}

function legacyDiagramUpdate(): Uint8Array {
  const doc = new Y.Doc();
  Y.applyUpdate(doc, diagramUpdate());
  const diagram = doc.getXmlFragment(SPEC_FRAGMENT_NAME).get(2);
  if (!(diagram instanceof Y.XmlElement)) throw new Error("The legacy design section is missing");
  const block = diagram.get(1);
  if (!(block instanceof Y.XmlElement)) throw new Error("The legacy diagram block is missing");
  const id = block.getAttribute("id");
  if (typeof id !== "string") throw new Error("The legacy diagram id is missing");
  block.setAttribute("blockId", id);
  block.removeAttribute("id");
  return Y.encodeStateAsUpdate(doc);
}

function diagramDocumentWithIds(ids: readonly string[]): Uint8Array {
  const template = createTemplateDocument(TEMPLATE);
  const sections = template.content.content.map((section) => {
    if (section.attrs.id !== "design") return section;
    return section.type.create(section.attrs, [
      section.firstChild!,
      ...ids.map((id) =>
        schema.nodes.diagramBlock!.create({ id, kind: "mermaid", source: "flowchart LR" }),
      ),
    ]);
  });
  return encodeProseMirrorDocument(schema.nodes.doc!.create(null, sections));
}

function changeFirstDiagramId(base: Uint8Array, id: string): Uint8Array {
  const doc = new Y.Doc();
  Y.applyUpdate(doc, base);
  const before = Y.encodeStateVector(doc);
  const section = doc.getXmlFragment(SPEC_FRAGMENT_NAME).get(2);
  if (!(section instanceof Y.XmlElement)) throw new Error("The design section is missing");
  const block = section.get(1);
  if (!(block instanceof Y.XmlElement)) throw new Error("The diagram block is missing");
  block.setAttribute("id", id);
  return Y.encodeStateAsUpdate(doc, before);
}

function withDiagramCaches(
  document: ReturnType<typeof proseMirrorDocument>,
  svg: (index: number) => string,
) {
  const positions: number[] = [];
  document.descendants((node, position) => {
    if (node.type === schema.nodes.diagramBlock) positions.push(position);
  });
  let transform = new Transform(document);
  for (const [index, position] of positions.entries()) {
    const node = transform.doc.nodeAt(position);
    if (!node) throw new Error("The diagram test block is missing");
    transform = transform.setNodeMarkup(position, undefined, {
      ...node.attrs,
      cachedRender: {
        kind: node.attrs.kind,
        source: node.attrs.source,
        blockId: node.attrs.id,
        rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
        svg: svg(index),
      },
    });
  }
  return transform.doc;
}

function withFirstDiagramCache(document: ProseMirrorNode, cachedRender: unknown): ProseMirrorNode {
  let position: number | null = null;
  document.descendants((node, nodePosition) => {
    if (position === null && node.type === schema.nodes.diagramBlock) position = nodePosition;
  });
  if (position === null) throw new Error("The diagram test block is missing");
  const node = document.nodeAt(position);
  if (!node) throw new Error("The diagram test block is missing");
  return new Transform(document).setNodeMarkup(position, undefined, {
    ...node.attrs,
    cachedRender,
  }).doc;
}

function withFirstDiagramSource(document: ProseMirrorNode, source: string): ProseMirrorNode {
  let position: number | null = null;
  document.descendants((node, nodePosition) => {
    if (position === null && node.type === schema.nodes.diagramBlock) position = nodePosition;
  });
  if (position === null) throw new Error("The diagram test block is missing");
  const node = document.nodeAt(position);
  if (!node) throw new Error("The diagram test block is missing");
  return new Transform(document).setNodeMarkup(position, undefined, {
    ...node.attrs,
    source,
    cachedRender: null,
  }).doc;
}

function clientInsert(base: Uint8Array, sectionIndex: number, value: string): Uint8Array {
  const doc = new Y.Doc();
  Y.applyUpdate(doc, base);
  const vector = Y.encodeStateVector(doc);
  const section = doc.getXmlFragment("prosemirror").get(sectionIndex);
  if (!(section instanceof Y.XmlElement)) throw new Error("The test section is missing");
  const block = section.get(1);
  if (!(block instanceof Y.XmlElement)) throw new Error("The test paragraph is missing");
  const text = new Y.XmlText();
  text.insert(0, value);
  block.insert(0, [text]);
  return Y.encodeStateAsUpdate(doc, vector);
}

function cacheOnlyUpdate(base: Uint8Array, value: string): Uint8Array {
  const doc = new Y.Doc();
  Y.applyUpdate(doc, base);
  const vector = Y.encodeStateVector(doc);
  doc.getMap<string>("renderer-cache").set("markdown", value);
  return Y.encodeStateAsUpdate(doc, vector);
}

function documentWithContextText(document: ProseMirrorNode, value: string): ProseMirrorNode {
  const section = findSection(document, "context");
  if (!section?.node.firstChild) throw new Error("The context section is missing");
  return replaceSection(
    document,
    "context",
    section.node.type.create(section.node.attrs, [
      section.node.firstChild,
      schema.nodes.paragraph!.create(null, value.length > 0 ? schema.text(value) : undefined),
    ]),
  );
}

async function seededService(
  store = new MemoryDocumentStore(),
  base = initialUpdate(),
): Promise<SpecDocumentService> {
  const service = new SpecDocumentService(store);
  await service.applyUpdate(SPEC_ID, base, "seed");
  return service;
}

function stateVector(serviceDoc: Y.Doc): number[] {
  return [...Y.encodeStateVector(serviceDoc)];
}

async function waitFor(predicate: () => boolean | Promise<boolean>): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (await predicate()) return;
    await Bun.sleep(10);
  }
  throw new Error("Timed out while waiting for spec document sync");
}

describe("shared spec document", () => {
  test("markdown round trips for the built-in template", () => {
    const markdown = `## Context

The system has a durable document.

## Requirements

### Functional

R1 keeps the document.

## Design

\`\`\`mermaid
flowchart LR
  A --> B
\`\`\`
`;
    const once = renderMarkdown(parseMarkdown(markdown, TEMPLATE));
    const twice = renderMarkdown(parseMarkdown(once, TEMPLATE));
    expect(twice).toBe(once);
  });

  test("the update growth bound covers worst-case escaping and structure", () => {
    const escaped = "\\`*_{}[]<>".repeat(2_000);
    const blocks = Array.from({ length: 200 }, (_, index) =>
      index % 2 === 0
        ? schema.nodes.heading!.create({ level: 6 }, schema.text(escaped))
        : schema.nodes.paragraph!.create(null, schema.text(escaped)),
    );
    blocks.push(
      schema.nodes.codeBlock!.create({ language: escaped }, schema.text(escaped)),
      schema.nodes.diagramBlock!.create({
        id: "diagram-1",
        kind: "mermaid",
        source: escaped,
      }),
      schema.nodes.paragraph!.create(null, [
        schema.text(escaped),
        schema.nodes.openQuestion!.create({ questionId: escaped }),
      ]),
    );
    const document = schema.nodes.doc!.create(null, [
      schema.nodes.section!.create({ id: "context", templateSectionKey: "context" }, [
        schema.nodes.sectionHeading!.create(null, schema.text(escaped)),
        ...blocks,
      ]),
    ]);
    const update = encodeProseMirrorDocument(document);
    const renderedBytes = new TextEncoder().encode(renderMarkdown(document)).byteLength;
    expect(renderedBytes).toBeLessThanOrEqual(update.byteLength * SPEC_UPDATE_SIZE_FACTOR);
  });
});

describe("SpecDocumentService", () => {
  test("a base-version Yjs update migrates blockId without changing block identity", async () => {
    const store = new MemoryDocumentStore();
    store.seedUpdate(SPEC_ID, legacyDiagramUpdate());

    const first = new SpecDocumentService(store);
    const loaded = await first.loadDoc(SPEC_ID);
    const blocks: Array<{ id: unknown; source: unknown }> = [];
    proseMirrorDocument(loaded.doc).descendants((node) => {
      if (node.type === schema.nodes.diagramBlock) {
        blocks.push({ id: node.attrs.id, source: node.attrs.source });
      }
    });
    expect(blocks).toEqual([{ id: "diagram-0", source: "flowchart LR\n  A0 --> B0" }]);
    expect(loaded.semanticDocSeq).toBe(1n);
    expect(store.updates.get(SPEC_ID)).toHaveLength(2);

    first.evict(SPEC_ID);
    const reloaded = await new SpecDocumentService(store).loadDoc(SPEC_ID);
    const diagram = findSection(proseMirrorDocument(reloaded.doc), "design")?.node.child(1);
    expect(diagram?.type).toBe(schema.nodes.diagramBlock);
    expect(diagram?.attrs.id).toBe("diagram-0");
    expect(reloaded.semanticDocSeq).toBe(1n);
  });

  test("the shared channel envelope is typed and rejects malformed payloads", () => {
    const awareness = encodeSpecChannelEnvelope({
      type: "awareness",
      specId: SPEC_ID,
      update: "encoded-awareness",
    });
    expect(parseSpecChannelEnvelope(awareness)).toEqual({
      type: "awareness",
      specId: SPEC_ID,
      update: "encoded-awareness",
    });
    const query = encodeSpecChannelEnvelope({ type: "awareness-query", specId: SPEC_ID });
    expect(parseSpecChannelEnvelope(query)).toEqual({
      type: "awareness-query",
      specId: SPEC_ID,
    });
    expect(parseSpecChannelEnvelope(`${SPEC_ID}:12`)).toBeNull();
    expect(parseSpecChannelEnvelope('{"type":"update"}')).toBeNull();
    expect(() =>
      encodeSpecChannelEnvelope({
        type: "awareness",
        specId: SPEC_ID,
        update: "x".repeat(8_000),
      }),
    ).toThrow("larger than 7900 bytes");
  });

  test("concurrent updates from three clients converge in every apply order", async () => {
    const base = initialUpdate();
    const updates = [
      clientInsert(base, 0, "alpha"),
      clientInsert(base, 1, "bravo"),
      clientInsert(base, 2, "charlie"),
    ];
    const orders = [
      [0, 1, 2],
      [2, 0, 1],
      [1, 2, 0],
    ];
    const documents: Y.Doc[] = [];
    for (const order of orders) {
      const service = await seededService(new MemoryDocumentStore(), base);
      for (const index of order) {
        await service.applyUpdate(SPEC_ID, updates[index]!, `client-${index}`);
      }
      documents.push((await service.loadDoc(SPEC_ID)).doc);
    }
    expect(stateVector(documents[1]!)).toEqual(stateVector(documents[0]!));
    expect(stateVector(documents[2]!)).toEqual(stateVector(documents[0]!));
  });

  test("snapshot plus tail rebuild equals the live document", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    await service.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await service.loadDoc(SPEC_ID)).doc), 0, "first"),
      "first",
    );
    await service.compact(SPEC_ID);
    await service.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await service.loadDoc(SPEC_ID)).doc), 1, "tail"),
      "tail",
    );

    const rebuilt = new SpecDocumentService(store);
    const liveDoc = (await service.loadDoc(SPEC_ID)).doc;
    const rebuiltDoc = (await rebuilt.loadDoc(SPEC_ID)).doc;
    expect(renderMarkdown(proseMirrorDocument(rebuiltDoc))).toBe(
      renderMarkdown(proseMirrorDocument(liveDoc)),
    );
    expect(stateVector(rebuiltDoc)).toEqual(stateVector(liveDoc));
  });

  test("a newer compacted snapshot refreshes the rendered size bound", async () => {
    const store = new MemoryDocumentStore();
    const first = await seededService(store);
    const second = new SpecDocumentService(store);
    await second.loadDoc(SPEC_ID);

    const baseDocument = proseMirrorDocument((await first.loadDoc(SPEC_ID)).doc);
    const emptySize = new TextEncoder().encode(
      renderMarkdown(documentWithContextText(baseDocument, "")),
    ).byteLength;
    const escapedCharacters = Math.floor((SPEC_MAX_SIZE_BYTES - emptySize - 100) / 2);
    await first.mutateDocument(SPEC_ID, "large-client", (document) =>
      documentWithContextText(document, "*".repeat(escapedCharacters)),
    );
    const compacted = await first.compact(SPEC_ID);
    expect(new TextEncoder().encode(compacted.renderedMarkdown).byteLength).toBeLessThan(
      SPEC_MAX_SIZE_BYTES,
    );

    await second.syncFromLog(SPEC_ID);
    const tail = clientInsert(
      Y.encodeStateAsUpdate((await second.loadDoc(SPEC_ID)).doc),
      0,
      "*".repeat(100),
    );
    await expect(second.applyUpdate(SPEC_ID, tail, "tail-client")).rejects.toBeInstanceOf(
      SpecDocumentTooLargeError,
    );
  });

  test("compaction preserves state and is idempotent", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    const first = await service.compact(SPEC_ID);
    const second = await service.compact(SPEC_ID);
    const rebuilt = await new SpecDocumentService(store).loadDoc(SPEC_ID);

    expect(second.renderedMarkdown).toBe(first.renderedMarkdown);
    expect(second.coveredSeq).toBe(first.coveredSeq);
    expect(store.compactions).toBe(2);
    expect(store.updates.get(SPEC_ID)).toHaveLength(0);
    expect(renderMarkdown(proseMirrorDocument(rebuilt.doc))).toBe(first.renderedMarkdown);
  });

  test("an update survives a failure between persistence and broadcast", async () => {
    const store = new MemoryDocumentStore();
    await seededService(store);
    let broadcasts = 0;
    const failed = new SpecDocumentService(store, {
      afterPersist: () => {
        throw new Error("simulated process exit");
      },
    });
    const loaded = await failed.loadDoc(SPEC_ID);
    failed.subscribe(SPEC_ID, () => {
      broadcasts += 1;
    });
    const update = clientInsert(Y.encodeStateAsUpdate(loaded.doc), 0, "durable");

    await expect(failed.applyUpdate(SPEC_ID, update, "client")).rejects.toThrow(
      "simulated process exit",
    );
    expect(broadcasts).toBe(0);

    const reloaded = await new SpecDocumentService(store).loadDoc(SPEC_ID);
    expect(renderMarkdown(proseMirrorDocument(reloaded.doc))).toContain("durable");
  });

  test("peer notifications and reconnects sync only the local working set", async () => {
    const ignoredSpecId = "00000000-0000-4000-8000-000000000109";
    const store = new MemoryDocumentStore();
    const service = new SpecDocumentService(store);
    await service.applyUpdate(SPEC_ID, initialUpdate(), "cached");
    await service.applyUpdate(ignoredSpecId, initialUpdate(), "ignored");
    service.evict(ignoredSpecId);
    await service.startPeerSync();
    store.tailReads.length = 0;

    await store.notifyUpdate(ignoredSpecId);
    await Bun.sleep(0);
    expect(store.tailReads).not.toContain(ignoredSpecId);

    await store.notifyUpdate(SPEC_ID);
    await waitFor(() => store.tailReads.includes(SPEC_ID));

    store.tailReads.length = 0;
    store.reconnect();
    await waitFor(() => store.tailReads.length > 0);
    expect(store.tailReads).toContain(SPEC_ID);
    expect(store.tailReads).not.toContain(ignoredSpecId);
    await service.stopPeerSync();
  });

  test("an update over 2 MB is rejected before persistence", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    const doc = (await service.loadDoc(SPEC_ID)).doc;
    const update = clientInsert(Y.encodeStateAsUpdate(doc), 0, "x".repeat(2 * 1024 * 1024 + 1));

    await expect(service.applyUpdate(SPEC_ID, update, "large-client")).rejects.toBeInstanceOf(
      SpecDocumentTooLargeError,
    );
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("a diagram source cannot make the document exceed 2 MB", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());

    await expect(
      service.mutateDocument(SPEC_ID, "large-source-client", (document) =>
        withFirstDiagramSource(document, "x".repeat(SPEC_MAX_SIZE_BYTES + 1)),
      ),
    ).rejects.toBeInstanceOf(SpecDocumentTooLargeError);
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("a cache-only update cannot exceed the per-block cache limit", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());

    await expect(
      service.mutateDocument(SPEC_ID, "cache-client", (document) =>
        withDiagramCaches(document, () => "x".repeat(SPEC_BLOCK_CACHE_MAX_BYTES + 1)),
      ),
    ).rejects.toBeInstanceOf(SpecBlockCacheTooLargeError);
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("a large unknown cache field counts against the raw per-block limit", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());

    await expect(
      service.mutateDocument(SPEC_ID, "cache-client", (document) =>
        withFirstDiagramCache(document, {
          kind: "mermaid",
          source: "flowchart LR",
          blockId: "diagram-0",
          rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
          svg: "<svg />",
          extra: "x".repeat(SPEC_BLOCK_CACHE_MAX_BYTES),
        }),
      ),
    ).rejects.toBeInstanceOf(SpecBlockCacheTooLargeError);
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("malformed cached renders are rejected before persistence", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());

    await expect(
      service.mutateDocument(SPEC_ID, "cache-client", (document) =>
        withFirstDiagramCache(document, {
          kind: "mermaid",
          blockId: "diagram-0",
          rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
          svg: "<svg />",
        }),
      ),
    ).rejects.toThrow("must contain only");
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("cache-only updates cannot make the complete Yjs state exceed 2 MB", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate(5));

    await expect(
      service.mutateDocument(SPEC_ID, "cache-client", (document) =>
        withDiagramCaches(document, (index) =>
          String(index).repeat(SPEC_BLOCK_CACHE_MAX_BYTES - 256),
        ),
      ),
    ).rejects.toMatchObject({
      name: "SpecDocumentTooLargeError",
      representation: "encoded Yjs state",
    });
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("a cache-only update has no section or human-digest effect", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());

    await service.mutateDocument(SPEC_ID, "human-client", (document) =>
      withDiagramCaches(document, () => '<svg><path d="M0 0" /></svg>'),
    );

    expect(store.lastEffects?.sections.every((section) => !section.changed)).toBe(true);
    expect(store.lastEffects?.at).toBeUndefined();
  });

  test("small updates use the conservative size fast path", async () => {
    const store = new MemoryDocumentStore();
    let exactMeasurements = 0;
    const service = new SpecDocumentService(store, {
      measureRenderedSize: (doc) => {
        exactMeasurements += 1;
        return new TextEncoder().encode(renderMarkdown(doc)).byteLength;
      },
    });
    await service.applyUpdate(SPEC_ID, initialUpdate(), "seed");
    expect(exactMeasurements).toBe(1);

    await service.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await service.loadDoc(SPEC_ID)).doc), 0, "small"),
      "client",
    );
    expect(exactMeasurements).toBe(1);
  });

  test("a schema-invalid Yjs update is rejected before persistence", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    const base = (await service.loadDoc(SPEC_ID)).doc;
    const hostile = new Y.Doc();
    Y.applyUpdate(hostile, Y.encodeStateAsUpdate(base));
    const before = Y.encodeStateVector(hostile);
    hostile.getXmlFragment("prosemirror").insert(0, [new Y.XmlElement("unknownNode")]);

    await expect(
      service.applyUpdate(SPEC_ID, Y.encodeStateAsUpdate(hostile, before), "hostile-client"),
    ).rejects.toThrow("violates the document schema");
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
    expect(renderMarkdown(proseMirrorDocument((await service.loadDoc(SPEC_ID)).doc))).toContain(
      "Context",
    );
  });

  test("a diagram block id cannot be empty", async () => {
    const store = new MemoryDocumentStore();
    const service = new SpecDocumentService(store);

    await expect(
      service.applyUpdate(SPEC_ID, diagramDocumentWithIds([""]), "hostile-client"),
    ).rejects.toThrow("non-empty id");
    expect(store.updates.get(SPEC_ID)).toBeUndefined();
  });

  test("diagram block ids must be globally unique", async () => {
    const store = new MemoryDocumentStore();
    const service = new SpecDocumentService(store);

    await expect(
      service.applyUpdate(
        SPEC_ID,
        diagramDocumentWithIds(["duplicate", "duplicate"]),
        "hostile-client",
      ),
    ).rejects.toThrow("duplicated");
    expect(store.updates.get(SPEC_ID)).toBeUndefined();
  });

  test("an existing Yjs diagram element cannot change its id", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store, diagramUpdate());
    const base = Y.encodeStateAsUpdate((await service.loadDoc(SPEC_ID)).doc);

    await expect(
      service.applyUpdate(SPEC_ID, changeFirstDiagramId(base, "changed-id"), "hostile-client"),
    ).rejects.toThrow("cannot change");
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("central validation reports only the sections changed by a client update", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);

    await service.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await service.loadDoc(SPEC_ID)).doc), 0, "changed"),
      "human-client",
    );

    expect(store.lastEffects?.sections.filter((section) => section.changed)).toEqual([
      { id: "context", title: "Context", changed: true },
    ]);
  });

  test("central validation rejects a requirement ID removed by a semantic mutation", async () => {
    const store = new MemoryDocumentStore();
    const initial = createTemplateDocument(TEMPLATE);
    const requirements = findSection(initial, "requirements");
    if (!requirements) throw new Error("The test Requirements section is missing.");
    const withRequirement = replaceSection(
      initial,
      "requirements",
      schema.nodes.section!.create(requirements.node.attrs, [
        requirements.node.firstChild!,
        schema.nodes.paragraph!.create(null, schema.text("R1: Keep stable identifiers.")),
      ]),
    );
    const service = await seededService(store, encodeProseMirrorDocument(withRequirement));

    await expect(
      service.mutateDocument(SPEC_ID, "tool-client", (doc) => {
        const section = findSection(doc, "requirements");
        if (!section) throw new Error("The test Requirements section is missing.");
        return replaceSection(
          doc,
          "requirements",
          schema.nodes.section!.create(section.node.attrs, [
            section.node.firstChild!,
            schema.nodes.paragraph!.create(null, schema.text("The identifier disappeared.")),
          ]),
        );
      }),
    ).rejects.toBeInstanceOf(RequirementIntegrityError);
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
  });

  test("a semantic mutation rejects a stale expected document revision", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    let mutationCalls = 0;

    await expect(
      service.mutateDocument(
        SPEC_ID,
        "tool-client",
        (doc) => {
          mutationCalls += 1;
          return doc;
        },
        0n,
      ),
    ).rejects.toBeInstanceOf(SpecDocumentRevisionConflictError);
    expect(mutationCalls).toBe(0);
  });

  test("restore saves a checkpoint and applies a forward section edit", async () => {
    const documentStore = new MemoryDocumentStore();
    const checkpointStore = new MemoryCheckpointStore();
    documentStore.checkpointSink = checkpointStore.checkpoints;
    const documents = await seededService(documentStore);
    const checkpoints = new SpecCheckpointService(documents, checkpointStore);
    await documents.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await documents.loadDoc(SPEC_ID)).doc), 0, "old"),
      "old-client",
    );
    const old = await checkpoints.createCheckpoint(SPEC_ID, {
      reason: "run_completed",
      label: "Old state",
    });
    await documents.applyUpdate(
      SPEC_ID,
      clientInsert(Y.encodeStateAsUpdate((await documents.loadDoc(SPEC_ID)).doc), 0, "new"),
      "new-client",
    );

    const result = await checkpoints.restoreSection(SPEC_ID, old.id, "context");
    if (!result.applied) throw new Error("The first restore must apply");
    const markdown = renderMarkdown(proseMirrorDocument((await documents.loadDoc(SPEC_ID)).doc));
    expect(markdown).toContain("old");
    expect(markdown).not.toContain("new");
    expect(result.update.seq).toBeGreaterThan(result.checkpointBeforeRestore.docSeq);
    expect(checkpointStore.checkpoints.size).toBe(2);

    const updateCountBeforeRetry = documentStore.updates.get(SPEC_ID)?.length ?? 0;
    const retry = await checkpoints.restoreSection(SPEC_ID, old.id, "context");
    expect(retry).toEqual({
      applied: false,
      checkpointBeforeRestore: null,
      update: null,
      docSeq: result.update.seq,
    });
    expect(checkpointStore.checkpoints.size).toBe(2);
    expect(documentStore.updates.get(SPEC_ID)).toHaveLength(updateCountBeforeRetry);
  });
});

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let livePool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 6 }) : null;
let liveDbReachable = false;

if (livePool) {
  liveDbReachable = await livePool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

describe("SpecDocumentService with live Postgres", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const peerSpecId = randomUUID();
  const userId = `spec-doc-test-${randomUUID()}`;
  const humanClientId = `human-client-${randomUUID()}`;
  const initialUpdatedAt = new Date("2026-08-09T10:00:00.000Z");
  const peerUpdatedAt = new Date("2026-08-09T11:00:00.000Z");

  beforeAll(async () => {
    if (!liveDbReachable || !livePool) return;
    await livePool.query(
      `INSERT INTO spec_template
         (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Test template', '[]', '[]', '{}')`,
      [templateId],
    );
    await livePool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle, updated_at)
       VALUES ($1, 'test-org', $2, 'Test spec', 'draft', $3),
              ($4, 'test-org', $2, 'Peer spec', 'draft', $5)`,
      [specId, templateId, initialUpdatedAt, peerSpecId, peerUpdatedAt],
    );
    await livePool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Spec document test', $2, false, $3, $3)`,
      [userId, `${userId}@example.invalid`, new Date("2026-08-09T12:00:00.000Z")],
    );
  }, 15_000);

  beforeEach(async () => {
    if (!liveDbReachable || !livePool) return;
    await livePool.query("DELETE FROM spec_transcript_action WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_section_state WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_participant WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_checkpoint WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_projection WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_update_log WHERE spec_id = $1", [specId]);
    await livePool.query("DELETE FROM spec_snapshot WHERE spec_id = $1", [specId]);
    await livePool.query(
      `UPDATE spec
          SET current_doc_seq = 0,
              current_semantic_doc_seq = 0,
              lifecycle = 'draft',
              updated_at = CASE WHEN id = $1 THEN $3::timestamptz ELSE $4::timestamptz END
        WHERE id = ANY($2::uuid[])`,
      [specId, [specId, peerSpecId], initialUpdatedAt, peerUpdatedAt],
    );
  }, 15_000);

  afterAll(async () => {
    if (!livePool) return;
    if (liveDbReachable) {
      await livePool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [[specId, peerSpecId]]);
      await livePool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
      await livePool.query(`DELETE FROM "user" WHERE id = $1`, [userId]);
    }
    await livePool.end();
    livePool = null;
  }, 15_000);

  test.skipIf(!liveDbReachable)(
    "a tracked edit commits its update, result, and transcript action atomically",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      let stopOnce = true;
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => new Date("2026-08-09T12:03:00.000Z"),
        afterPersist: (_storedSpecId, seq) => {
          if (!stopOnce || seq !== 2n) return;
          stopOnce = false;
          throw new Error("simulated stop after tracked-edit commit");
        },
      });
      await documents.applyUpdate(specId, initialUpdate(), "seed");
      const action = {
        id: `selection-edit:${specId}:session:call`,
        specId,
        sectionId: "context",
        requestFingerprint: "request-fingerprint",
        concurrentEditors: ["Sam"],
        chip: {
          kind: "spec_tracked_edit" as const,
          specId,
          sectionId: "context",
          before: "",
          after: "Tracked edit",
        },
      };

      await expect(
        documents.mutateDocumentWithTrackedEdit(
          specId,
          "agent:session:call",
          action,
          (document) => {
            const section = findSection(document, "context");
            if (!section) throw new Error("The context section is missing.");
            return new Transform(document).insert(
              section.position + section.node.nodeSize - 2,
              schema.text("Tracked edit"),
            ).doc;
          },
        ),
      ).rejects.toThrow("simulated stop after tracked-edit commit");

      const committed = await livePool.query<{
        current_doc_seq: string;
        updates: string;
        actions: string;
        stored_rev: string;
      }>(
        `SELECT s.current_doc_seq::text,
                (SELECT count(*)::text FROM spec_update_log u WHERE u.spec_id = s.id) AS updates,
                (SELECT count(*)::text FROM spec_transcript_action a WHERE a.spec_id = s.id) AS actions,
                (SELECT a.result->>'newRev' FROM spec_transcript_action a WHERE a.id = $2) AS stored_rev
           FROM spec s
          WHERE s.id = $1`,
        [specId, action.id],
      );
      expect(committed.rows[0]).toEqual({
        current_doc_seq: "2",
        updates: "2",
        actions: "1",
        stored_rev: "2",
      });

      const replay = await documents.mutateDocumentWithTrackedEdit(
        specId,
        "agent:session:call",
        action,
        () => {
          throw new Error("a replay must not resolve the changed range");
        },
      );
      expect(replay.status).toBe("replayed");
      expect(replay.action.result).toEqual({
        applied: true,
        newRev: 2n,
        concurrentEditors: ["Sam"],
        transcriptChip: action.chip,
      });
    },
  );

  test.skipIf(!liveDbReachable)(
    "a stale draft update cannot persist after publication",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => new Date("2026-08-10T02:00:00.000Z"),
      });
      await documents.applyUpdate(specId, initialUpdate(), "seed");
      const staleUpdate = clientInsert(
        Y.encodeStateAsUpdate((await documents.loadDoc(specId)).doc),
        0,
        "stale draft edit",
      );
      await livePool.query("UPDATE spec SET lifecycle = 'published' WHERE id = $1", [specId]);

      await expect(
        documents.applyUpdate(specId, staleUpdate, "stale-client"),
      ).rejects.toBeInstanceOf(SpecDocumentReadOnlyError);
      const durable = await livePool.query<{ current_doc_seq: string; updates: string }>(
        `SELECT current_doc_seq::text,
                (SELECT count(*)::text FROM spec_update_log WHERE spec_id = $1) AS updates
           FROM spec
          WHERE id = $1`,
        [specId],
      );
      expect(durable.rows[0]).toEqual({ current_doc_seq: "1", updates: "1" });
    },
  );

  test.skipIf(!liveDbReachable)(
    "a restore recovery checkpoint includes an interleaved edit",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const now = () => new Date("2026-08-10T03:00:00.000Z");
      const checkpointStore = new PostgresSpecCheckpointStore(livePool);
      const seedDocuments = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now,
      });
      await seedDocuments.applyUpdate(specId, initialUpdate(), "seed");
      await seedDocuments.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await seedDocuments.loadDoc(specId)).doc), 0, "old"),
        "old-client",
      );
      const seedCheckpoints = new SpecCheckpointService(seedDocuments, checkpointStore);
      const source = await seedCheckpoints.createCheckpoint(specId, {
        reason: "run_completed",
        label: "Old state",
      });
      await seedDocuments.applyUpdate(
        specId,
        clientInsert(
          Y.encodeStateAsUpdate((await seedDocuments.loadDoc(specId)).doc),
          0,
          "current",
        ),
        "current-client",
      );

      let enterPersist = () => {};
      const persistEntered = new Promise<void>((resolve) => {
        enterPersist = resolve;
      });
      let releasePersist = () => {};
      const persistReleased = new Promise<void>((resolve) => {
        releasePersist = resolve;
      });
      let pauseOnce = true;
      const restoringDocuments = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now,
        beforeCheckpointPersist: async () => {
          if (!pauseOnce) return;
          pauseOnce = false;
          enterPersist();
          await persistReleased;
        },
      });
      const concurrentDocuments = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now,
      });
      await restoringDocuments.loadDoc(specId);
      await concurrentDocuments.loadDoc(specId);
      const restoringCheckpoints = new SpecCheckpointService(restoringDocuments, checkpointStore);
      const restore = restoringCheckpoints.restoreSection(specId, source.id, "context");
      await persistEntered;

      await concurrentDocuments.applyUpdate(
        specId,
        clientInsert(
          Y.encodeStateAsUpdate((await concurrentDocuments.loadDoc(specId)).doc),
          2,
          "peer edit",
        ),
        "peer-client",
      );
      releasePersist();
      const result = await restore;
      if (!result.applied) throw new Error("The interleaved restore must apply");

      const recoveryDoc = new Y.Doc();
      Y.applyUpdate(recoveryDoc, result.checkpointBeforeRestore.state);
      const recoveryMarkdown = renderMarkdown(proseMirrorDocument(recoveryDoc));
      recoveryDoc.destroy();
      expect(recoveryMarkdown).toContain("current");
      expect(recoveryMarkdown).toContain("peer edit");
      expect(result.checkpointBeforeRestore.docSeq).toBe(4n);
      expect(result.update.seq).toBe(5n);

      const finalMarkdown = renderMarkdown(
        proseMirrorDocument((await restoringDocuments.loadDoc(specId)).doc),
      );
      expect(finalMarkdown).toContain("old");
      expect(finalMarkdown).toContain("peer edit");
      expect(finalMarkdown).not.toContain("current");
    },
  );

  test.skipIf(!liveDbReachable)(
    "migration 0059 drops the rollout triggers and rejects writers that omit semantic revisions",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const client = await livePool.connect();
      const schemaName = `semantic_revision_${randomUUID().replaceAll("-", "")}`;
      const quotedSchema = `"${schemaName}"`;
      const migrationSpecId = randomUUID();

      const applyMigration = async (file: string) => {
        const migration = await readFile(
          new URL(`../../../drizzle/${file}`, import.meta.url),
          "utf8",
        );
        for (const statement of migration
          .split("--> statement-breakpoint")
          .map((value) => value.trim())
          .filter(Boolean)) {
          await client.query(statement);
        }
      };

      // A rejected write aborts the enclosing transaction, so run each legacy
      // write inside a savepoint. Returns the SQLSTATE, or null when Postgres
      // accepted the write.
      const rejectionCode = async (statement: string, params: unknown[]) => {
        await client.query("SAVEPOINT legacy_write");
        try {
          await client.query(statement, params);
          return null;
        } catch (error) {
          return (error as { code?: string }).code ?? null;
        } finally {
          await client.query("ROLLBACK TO SAVEPOINT legacy_write");
        }
      };

      try {
        await client.query("BEGIN");
        await client.query(`CREATE SCHEMA ${quotedSchema}`);
        await client.query(`SET LOCAL search_path TO ${quotedSchema}`);
        await client.query(
          `CREATE TABLE spec (
             id uuid PRIMARY KEY,
             current_doc_seq bigint DEFAULT 0 NOT NULL
           );
           CREATE TABLE spec_update_log (
             spec_id uuid NOT NULL,
             seq bigint NOT NULL,
             update bytea NOT NULL,
             client_id text,
             PRIMARY KEY (spec_id, seq)
           );
           CREATE TABLE spec_snapshot (
             spec_id uuid PRIMARY KEY,
             state bytea NOT NULL,
             state_vector bytea NOT NULL,
             covered_seq bigint NOT NULL
           );
           CREATE TABLE spec_projection (
             spec_id uuid NOT NULL,
             rev bigint NOT NULL,
             session_id uuid NOT NULL,
             doc_seq bigint NOT NULL,
             PRIMARY KEY (spec_id, rev)
           )`,
        );
        await client.query("INSERT INTO spec (id, current_doc_seq) VALUES ($1, 2)", [
          migrationSpecId,
        ]);
        await client.query(
          `INSERT INTO spec_update_log (spec_id, seq, update, client_id)
           VALUES ($1, 2, ''::bytea, 'legacy')`,
          [migrationSpecId],
        );
        await client.query(
          `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq)
           VALUES ($1, ''::bytea, ''::bytea, 2)`,
          [migrationSpecId],
        );
        await client.query(
          `INSERT INTO spec_projection (spec_id, rev, session_id, doc_seq)
           VALUES ($1, 1, $1, 2)`,
          [migrationSpecId],
        );

        await applyMigration("0056_spec_semantic_revision.sql");
        // 0056 backfills the rows an older pod wrote.
        const backfilled = await client.query<{
          spec: string;
          update_log: string;
          snapshot: string;
          projection: string;
        }>(
          `SELECT current_semantic_doc_seq::text AS spec,
                  (SELECT max(semantic_doc_seq)::text FROM spec_update_log) AS update_log,
                  (SELECT covered_semantic_doc_seq::text FROM spec_snapshot) AS snapshot,
                  (SELECT semantic_doc_seq::text FROM spec_projection) AS projection
             FROM spec
            WHERE id = $1`,
          [migrationSpecId],
        );
        expect(backfilled.rows).toEqual([
          { spec: "2", update_log: "2", snapshot: "2", projection: "2" },
        ]);

        await applyMigration("0059_spec_semantic_revision_contract.sql");

        // No rollout trigger and no rollout function survives 0059.
        const leftovers = await client.query<{ triggers: string; functions: string }>(
          `SELECT (SELECT count(*)::text
                     FROM pg_trigger t
                     JOIN pg_class c ON c.oid = t.tgrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE NOT t.tgisinternal
                      AND n.nspname = $1
                      AND t.tgname LIKE '%legacy_semantic_revision%') AS triggers,
                  (SELECT count(*)::text
                     FROM pg_proc p
                     JOIN pg_namespace n ON n.oid = p.pronamespace
                    WHERE n.nspname = $1
                      AND p.proname LIKE '%legacy_semantic_revision%') AS functions`,
          [schemaName],
        );
        expect(leftovers.rows).toEqual([{ triggers: "0", functions: "0" }]);

        // The four semantic revision columns stay NOT NULL. That constraint is
        // what rejects a writer which omits a semantic revision.
        const nullability = await client.query<{ table_name: string; is_nullable: string }>(
          `SELECT table_name, is_nullable
             FROM information_schema.columns
            WHERE table_schema = $1
              AND ((table_name = 'spec' AND column_name = 'current_semantic_doc_seq')
                OR (table_name = 'spec_projection' AND column_name = 'semantic_doc_seq')
                OR (table_name = 'spec_snapshot' AND column_name = 'covered_semantic_doc_seq')
                OR (table_name = 'spec_update_log' AND column_name = 'semantic_doc_seq'))
            ORDER BY table_name`,
          [schemaName],
        );
        expect(nullability.rows).toEqual([
          { table_name: "spec", is_nullable: "NO" },
          { table_name: "spec_projection", is_nullable: "NO" },
          { table_name: "spec_snapshot", is_nullable: "NO" },
          { table_name: "spec_update_log", is_nullable: "NO" },
        ]);

        // An insert that omits the semantic revision is rejected (SQLSTATE
        // 23502, not_null_violation).
        expect(
          await rejectionCode(
            `INSERT INTO spec_update_log (spec_id, seq, update, client_id)
             VALUES ($1, 3, ''::bytea, 'legacy')`,
            [migrationSpecId],
          ),
        ).toBe("23502");
        expect(
          await rejectionCode(
            `INSERT INTO spec_projection (spec_id, rev, session_id, doc_seq)
             VALUES ($1, 2, $1, 3)`,
            [migrationSpecId],
          ),
        ).toBe("23502");
        // A snapshot upsert is rejected on both arms: Postgres checks NOT NULL
        // on the proposed row before it arbitrates the conflict, so the reject
        // does not depend on whether a snapshot row already exists.
        expect(
          await rejectionCode(
            `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq)
             VALUES ($1, ''::bytea, ''::bytea, 3)
             ON CONFLICT (spec_id) DO UPDATE SET covered_seq = excluded.covered_seq`,
            [migrationSpecId],
          ),
        ).toBe("23502");
        expect(
          await rejectionCode(
            `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq)
             VALUES ($1, ''::bytea, ''::bytea, 3)
             ON CONFLICT (spec_id) DO UPDATE SET covered_seq = excluded.covered_seq`,
            [randomUUID()],
          ),
        ).toBe("23502");

        // An update that omits the semantic revision keeps a non-null value, so
        // NOT NULL cannot reject it. It now leaves the semantic revision behind
        // instead of advancing it. No constraint can tell that apart from a
        // legitimate non-semantic edit, which is why 0056 needed a session
        // setting to suppress its own trigger. The contract is that every
        // writer states the semantic revision, and every pod now does.
        await client.query("UPDATE spec SET current_doc_seq = 3 WHERE id = $1", [migrationSpecId]);
        await client.query(
          "UPDATE spec_projection SET doc_seq = 3 WHERE spec_id = $1 AND rev = 1",
          [migrationSpecId],
        );
        const stale = await client.query<{ spec: string; projection: string }>(
          `SELECT current_semantic_doc_seq::text AS spec,
                  (SELECT semantic_doc_seq::text FROM spec_projection) AS projection
             FROM spec
            WHERE id = $1`,
          [migrationSpecId],
        );
        expect(stale.rows).toEqual([{ spec: "2", projection: "2" }]);

        // A current writer states both revisions and needs no session setting.
        await client.query(
          `UPDATE spec
              SET current_doc_seq = 4,
                  current_semantic_doc_seq = 3
            WHERE id = $1`,
          [migrationSpecId],
        );
        await client.query(
          `INSERT INTO spec_update_log (spec_id, seq, semantic_doc_seq, update, client_id)
           VALUES ($1, 4, 3, ''::bytea, 'current')`,
          [migrationSpecId],
        );
        await client.query(
          `INSERT INTO spec_snapshot (spec_id, state, state_vector, covered_seq, covered_semantic_doc_seq)
           VALUES ($1, ''::bytea, ''::bytea, 4, 3)
           ON CONFLICT (spec_id) DO UPDATE
           SET covered_seq = excluded.covered_seq,
               covered_semantic_doc_seq = excluded.covered_semantic_doc_seq`,
          [migrationSpecId],
        );
        await client.query(
          `UPDATE spec_projection
              SET doc_seq = 4,
                  semantic_doc_seq = 3
            WHERE spec_id = $1 AND rev = 1`,
          [migrationSpecId],
        );
        const current = await client.query<{
          spec: string;
          update_log: string;
          snapshot: string;
          projection: string;
        }>(
          `SELECT current_semantic_doc_seq::text AS spec,
                  (SELECT max(semantic_doc_seq)::text FROM spec_update_log) AS update_log,
                  (SELECT covered_semantic_doc_seq::text FROM spec_snapshot) AS snapshot,
                  (SELECT semantic_doc_seq::text FROM spec_projection) AS projection
             FROM spec
            WHERE id = $1`,
          [migrationSpecId],
        );
        expect(current.rows).toEqual([
          { spec: "3", update_log: "3", snapshot: "3", projection: "3" },
        ]);
      } finally {
        await client.query("ROLLBACK");
        client.release();
      }
    },
  );

  test.skipIf(!liveDbReachable)(
    "a document mutation commits its attributed checkpoint in the same transaction",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const checkpointId = randomUUID();
      const at = new Date("2026-08-09T12:03:00.000Z");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => at,
      });
      await documents.applyUpdate(specId, initialUpdate(), "seed");

      const stored = await documents.mutateDocumentWithPostEditCheckpoint(
        specId,
        "spec-agent:test",
        {
          id: checkpointId,
          label: "Updated block request-flow",
          authorUserId: userId,
          reason: "block_edit",
          createdAt: at,
        },
        (document) => {
          const section = findSection(document, "design");
          if (!section) throw new Error("The Design section is missing.");
          return replaceSection(
            document,
            "design",
            schema.nodes.section!.create(section.node.attrs, [
              section.node.firstChild!,
              schema.nodes.paragraph!.create(null, schema.text("Checkpointed block edit.")),
            ]),
          );
        },
      );

      const checkpoint = await livePool.query<{
        author_user_id: string | null;
        created_at: Date;
        doc_seq: string;
        reason: string;
        rendered_markdown: string;
      }>(
        `SELECT author_user_id, created_at, doc_seq::text, reason, rendered_markdown
           FROM spec_checkpoint
          WHERE id = $1`,
        [checkpointId],
      );
      expect(checkpoint.rows[0]).toMatchObject({
        author_user_id: userId,
        created_at: at,
        doc_seq: stored.update.seq.toString(),
        reason: "block_edit",
      });
      expect(checkpoint.rows[0]?.rendered_markdown).toContain("Checkpointed block edit.");
    },
  );

  test.skipIf(!liveDbReachable)(
    "keeps only the newest automatic block-edit checkpoints",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => new Date("2026-08-09T12:03:00.000Z"),
      });
      await documents.applyUpdate(specId, initialUpdate(), "seed");
      const manualCheckpointId = randomUUID();
      const compacted = await documents.compact(specId);
      await livePool.query(
        `INSERT INTO spec_checkpoint
           (id, spec_id, state, state_vector, rendered_markdown, doc_seq,
            label, author_user_id, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)`,
        [
          manualCheckpointId,
          specId,
          Buffer.from(compacted.state),
          Buffer.from(compacted.stateVector),
          compacted.renderedMarkdown,
          compacted.coveredSeq.toString(),
          "Manual checkpoint",
          userId,
          "manual",
          new Date("2026-08-09T12:02:00.000Z"),
        ],
      );

      for (let index = 0; index < SPEC_BLOCK_EDIT_CHECKPOINT_LIMIT + 2; index += 1) {
        await documents.mutateDocumentWithPostEditCheckpoint(
          specId,
          "spec-agent:test",
          {
            id: randomUUID(),
            label: `Block edit ${index}`,
            authorUserId: userId,
            reason: "block_edit",
            createdAt: new Date(Date.UTC(2026, 7, 9, 12, 3, index)),
          },
          (document) => {
            const section = findSection(document, "design");
            if (!section) throw new Error("The Design section is missing.");
            return replaceSection(
              document,
              "design",
              schema.nodes.section!.create(section.node.attrs, [
                section.node.firstChild!,
                schema.nodes.paragraph!.create(null, schema.text(`Block edit ${index}.`)),
              ]),
            );
          },
        );
      }

      const counts = await livePool.query<{ block_edits: string; manual: string }>(
        `SELECT count(*) FILTER (WHERE reason = 'block_edit')::text AS block_edits,
                count(*) FILTER (WHERE reason = 'manual')::text AS manual
           FROM spec_checkpoint
          WHERE spec_id = $1`,
        [specId],
      );
      expect(counts.rows).toEqual([
        { block_edits: SPEC_BLOCK_EDIT_CHECKPOINT_LIMIT.toString(), manual: "1" },
      ]);
    },
  );

  test.skipIf(!liveDbReachable)("bounds the checkpoint history query", async () => {
    if (!livePool) throw new Error("The live Postgres pool is not available");
    const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
      now: () => new Date("2026-08-09T12:03:00.000Z"),
    });
    await documents.applyUpdate(specId, initialUpdate(), "seed");
    const compacted = await documents.compact(specId);
    await livePool.query(
      `INSERT INTO spec_checkpoint
         (id, spec_id, state, state_vector, rendered_markdown, doc_seq,
          label, author_user_id, reason, created_at)
       SELECT ('10000000-0000-4000-8000-' || lpad(value::text, 12, '0'))::uuid,
              $1, $2, $3, $4, $5, 'Checkpoint ' || value, $6, 'run_completed',
              $7::timestamptz + value * interval '1 second'
         FROM generate_series(1, 105) AS value`,
      [
        specId,
        Buffer.from(compacted.state),
        Buffer.from(compacted.stateVector),
        compacted.renderedMarkdown,
        compacted.coveredSeq.toString(),
        userId,
        new Date("2026-08-09T12:00:00.000Z"),
      ],
    );

    const checkpoints = await new PostgresSpecReadStore(livePool).listCheckpoints(specId);
    expect(checkpoints).toHaveLength(100);
    expect(checkpoints[0]?.label).toBe("Checkpoint 105");
    expect(checkpoints.at(-1)?.label).toBe("Checkpoint 6");
  });

  test.skipIf(!liveDbReachable)(
    "a human edit commits its update, drafted state, and transcript action atomically",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const connectedAt = new Date("2026-08-09T12:00:00.000Z");
      const leaseExpiresAt = new Date("2100-01-01T00:00:00.000Z");
      await livePool.query(
        `INSERT INTO spec_participant
           (spec_id, client_id, user_id, connection_epoch, connected_at, lease_expires_at)
         VALUES ($1, $2, $3, 1, $4, $5)`,
        [specId, humanClientId, userId, connectedAt, leaseExpiresAt],
      );

      const seeded = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => connectedAt,
      });
      await seeded.applyUpdate(specId, initialUpdate(), "seed");
      const humanUpdate = clientInsert(
        Y.encodeStateAsUpdate((await seeded.loadDoc(specId)).doc),
        0,
        "human edit",
      );
      const withoutClock = new SpecDocumentService(new PostgresSpecDocumentStore(livePool));
      await expect(withoutClock.applyUpdate(specId, humanUpdate, humanClientId)).rejects.toThrow(
        "injected timestamp",
      );
      const afterRollback = await livePool.query<{
        current_doc_seq: string;
        updates: string;
        states: string;
        actions: string;
      }>(
        `SELECT s.current_doc_seq::text,
                (SELECT count(*)::text FROM spec_update_log u WHERE u.spec_id = s.id) AS updates,
                (SELECT count(*)::text FROM spec_section_state st WHERE st.spec_id = s.id) AS states,
                (SELECT count(*)::text FROM spec_transcript_action a WHERE a.spec_id = s.id) AS actions
           FROM spec s
          WHERE s.id = $1`,
        [specId],
      );
      expect(afterRollback.rows[0]).toEqual({
        current_doc_seq: "1",
        updates: "1",
        states: "0",
        actions: "0",
      });

      const at = new Date("2026-08-09T12:01:00.000Z");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => at,
      });
      const stored = await documents.applyUpdate(specId, humanUpdate, humanClientId);
      expect(stored.seq).toBe(2n);
      const actionId = `human-edit:${specId}:2:context`;
      const committed = await livePool.query<{
        state: string;
        action_id: string;
        section_id: string;
      }>(
        `SELECT st.state, a.id AS action_id, a.chip->>'sectionId' AS section_id
           FROM spec_section_state st
           JOIN spec_transcript_action a
             ON a.spec_id = st.spec_id AND a.section_id = st.section_id
          WHERE st.spec_id = $1 AND st.section_id = 'context'`,
        [specId],
      );
      expect(committed.rows[0]).toEqual({
        state: "drafted",
        action_id: actionId,
        section_id: "context",
      });

      const stateStore = new PostgresSectionStateStore(livePool);
      const current = await stateStore.read(specId, "context");
      const confirmed = transitionSectionState(current, "confirmed", {
        specId,
        sectionId: "context",
        sectionTitle: "Context",
        allowsNa: true,
      });
      const later = await stateStore.persistStateAction({
        actionId: `confirm-after-human:${specId}`,
        specId,
        sectionId: "context",
        requestFingerprint: "confirm-after-human",
        expectedDocSeq: 2n,
        expected: current,
        next: confirmed.value,
        confirmedBy: userId,
        chip: confirmed.transcriptChip,
        at: new Date("2026-08-09T12:02:00.000Z"),
      });
      expect(later.status).toBe("stored");
      expect((await stateStore.read(specId, "context")).state).toBe("confirmed");
    },
  );

  test.skipIf(!liveDbReachable)(
    "a visible edit updates list time and order without cache-only churn",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      let clock = initialUpdatedAt;
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => clock,
      });
      const list = makeSpecListStore(drizzle(livePool, { schema: dbSchema }), () => clock);

      await documents.applyUpdate(specId, initialUpdate(), "seed");
      const before = await list.list({ orgId: "test-org", page: 1, pageSize: 50 });
      expect(before.rows.map((row) => row.id)).toEqual([peerSpecId, specId]);

      clock = new Date("2026-08-09T12:00:00.000Z");
      await documents.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await documents.loadDoc(specId)).doc), 0, "edit"),
        "agent-edit",
      );
      const afterEdit = await list.list({ orgId: "test-org", page: 1, pageSize: 50 });
      expect(afterEdit.rows.map((row) => row.id)).toEqual([specId, peerSpecId]);
      expect(afterEdit.rows[0]?.updatedAt).toEqual(clock);

      clock = new Date("2026-08-09T13:00:00.000Z");
      await documents.applyUpdate(
        specId,
        cacheOnlyUpdate(Y.encodeStateAsUpdate((await documents.loadDoc(specId)).doc), "cached"),
        "renderer-cache",
      );
      const afterCache = await list.list({ orgId: "test-org", page: 1, pageSize: 50 });
      expect(afterCache.rows[0]?.updatedAt).toEqual(new Date("2026-08-09T12:00:00.000Z"));
    },
  );

  test.skipIf(!liveDbReachable)(
    "a replacement participant epoch fences the stale socket from the update log",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const now = new Date("2026-08-09T12:00:00.000Z");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => now,
      });
      await documents.applyUpdate(specId, initialUpdate(), "seed");
      await livePool.query(
        `INSERT INTO spec_participant
           (spec_id, client_id, user_id, connection_epoch, connected_at, lease_expires_at)
         VALUES ($1, $2, $3, 1, $4, $5)`,
        [specId, humanClientId, userId, now, new Date(now.getTime() + 60_000)],
      );
      await livePool.query(
        `UPDATE spec_participant
            SET connection_epoch = 2,
                lease_expires_at = $3
          WHERE spec_id = $1 AND client_id = $2`,
        [specId, humanClientId, new Date(now.getTime() + 61_000)],
      );
      const update = clientInsert(
        Y.encodeStateAsUpdate((await documents.loadDoc(specId)).doc),
        0,
        "fenced edit",
      );

      await expect(documents.applyUpdate(specId, update, humanClientId, 1n)).rejects.toBeInstanceOf(
        SpecParticipantLeaseStaleError,
      );
      const afterStale = await livePool.query<{ current_doc_seq: string; updates: string }>(
        `SELECT current_doc_seq::text,
                (SELECT count(*)::text FROM spec_update_log WHERE spec_id = $1) AS updates
           FROM spec
          WHERE id = $1`,
        [specId],
      );
      expect(afterStale.rows).toEqual([{ current_doc_seq: "1", updates: "1" }]);

      const accepted = await documents.applyUpdate(specId, update, humanClientId, 2n);
      expect(accepted.seq).toBe(2n);
    },
  );

  test.skipIf(!liveDbReachable)(
    "a cache-only update preserves section state and the human transcript",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const stateTime = new Date("2026-08-09T12:03:00.000Z");
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => {
          throw new Error("A cache-only update must not request a human-edit timestamp");
        },
      });
      await documents.applyUpdate(specId, diagramUpdate(), null);
      await livePool.query(
        `INSERT INTO spec_participant (spec_id, client_id, user_id, connected_at)
         VALUES ($1, $2, $3, $4)`,
        [specId, humanClientId, userId, stateTime],
      );
      await livePool.query(
        `INSERT INTO spec_section_state
           (spec_id, section_id, state, na_reason, confirmed_by, updated_at)
         VALUES ($1, 'design', 'confirmed', NULL, $2, $3)`,
        [specId, userId, stateTime],
      );

      await documents.mutateDocument(specId, humanClientId, (document) =>
        withDiagramCaches(document, () => '<svg><path d="M0 0" /></svg>'),
      );

      const result = await livePool.query<{
        state: string;
        updated_at: Date;
        actions: string;
      }>(
        `SELECT state, updated_at,
                (SELECT count(*)::text
                   FROM spec_transcript_action
                  WHERE spec_id = $1) AS actions
           FROM spec_section_state
          WHERE spec_id = $1 AND section_id = 'design'`,
        [specId],
      );
      expect(result.rows[0]?.state).toBe("confirmed");
      expect(result.rows[0]?.updated_at.toISOString()).toBe(stateTime.toISOString());
      expect(result.rows[0]?.actions).toBe("0");
    },
  );

  test.skipIf(!liveDbReachable)(
    "spec_read, a cache write, and an agent mutation use one intended semantic projection",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      await livePool.query("UPDATE spec SET session_id = $2 WHERE id = $1", [specId, specId]);
      const documents = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), {
        now: () => new Date("2026-08-10T11:00:00.000Z"),
      });
      await documents.applyUpdate(specId, diagramUpdate(), null);
      const projections = new PostgresSpecProjectionStore(
        () => new Date("2026-08-10T11:00:00.000Z"),
      );
      const initialProjection = await projections.reserve({
        specId,
        sessionId: specId,
        source: "initial",
      });
      await projections.markPublished(specId, initialProjection.rev);

      const readRevision = (await documents.syncFromLog(specId)).semanticDocSeq;
      expect(readRevision).toBe(1n);
      const cacheUpdate = await documents.mutateDocument(specId, "cache-client", (document) =>
        withDiagramCaches(document, () => '<svg><path d="M0 0" /></svg>'),
      );
      expect(cacheUpdate.semanticDocSeq).toBe(readRevision);
      const afterCache = await projections.reserve({
        specId,
        sessionId: specId,
        source: "cache-only",
      });
      expect(afterCache.rev).toBe(initialProjection.rev);

      const agentUpdate = await documents.mutateDocument(
        specId,
        "agent-client",
        (document) => documentWithContextText(document, "agent edit"),
        readRevision,
      );
      expect(agentUpdate.semanticDocSeq).toBe(2n);
      const intendedProjection = await projections.reserve({
        specId,
        sessionId: specId,
        source: "agent-tool",
      });
      expect(intendedProjection.rev).toBe(initialProjection.rev + 1n);
      const revisions = await livePool.query<{
        current_doc_seq: string;
        current_semantic_doc_seq: string;
        projection_count: string;
      }>(
        `SELECT current_doc_seq::text,
                current_semantic_doc_seq::text,
                (SELECT count(*)::text FROM spec_projection WHERE spec_id = $1) AS projection_count
           FROM spec
          WHERE id = $1`,
        [specId],
      );
      expect(revisions.rows[0]).toEqual({
        current_doc_seq: "3",
        current_semantic_doc_seq: "2",
        projection_count: "2",
      });
    },
  );

  test.skipIf(!liveDbReachable)(
    "two instances converge after a dropped notification by filling the log gap",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const now = () => new Date("2026-08-09T12:00:00.000Z");
      const first = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), { now });
      const second = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), { now });
      await first.applyUpdate(specId, initialUpdate(), "seed");
      await second.loadDoc(specId);
      await second.startPeerSync();

      const firstUpdate = await first.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await first.loadDoc(specId)).doc), 0, "first"),
        "client-1",
      );
      await waitFor(async () => (await second.loadDoc(specId)).lastAppliedSeq >= firstUpdate.seq);

      await second.stopPeerSync();
      await first.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await first.loadDoc(specId)).doc), 1, "missed-a"),
        "client-2",
      );
      await first.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await first.loadDoc(specId)).doc), 2, "missed-b"),
        "client-3",
      );
      await second.startPeerSync();
      const final = await first.applyUpdate(
        specId,
        clientInsert(Y.encodeStateAsUpdate((await first.loadDoc(specId)).doc), 0, "wake"),
        "client-4",
      );
      await waitFor(async () => (await second.loadDoc(specId)).lastAppliedSeq >= final.seq);

      const firstDoc = (await first.loadDoc(specId)).doc;
      const secondDoc = (await second.loadDoc(specId)).doc;
      expect(stateVector(secondDoc)).toEqual(stateVector(firstDoc));
      expect(renderMarkdown(proseMirrorDocument(secondDoc))).toContain("missed-a");
      expect(renderMarkdown(proseMirrorDocument(secondDoc))).toContain("missed-b");
      await second.stopPeerSync();
    },
  );

  test.skipIf(!liveDbReachable)(
    "concurrent writers allocate dense committed revisions before compaction",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const now = () => new Date("2026-08-09T12:00:00.000Z");
      const first = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), { now });
      const second = new SpecDocumentService(new PostgresSpecDocumentStore(livePool), { now });
      await first.applyUpdate(specId, initialUpdate(), "seed");
      const common = Y.encodeStateAsUpdate((await first.loadDoc(specId)).doc);
      await second.loadDoc(specId);

      const lock = await livePool.connect();
      await lock.query("BEGIN");
      await lock.query("SELECT id FROM spec WHERE id = $1 FOR UPDATE", [specId]);
      const writes = Promise.all([
        first.applyUpdate(specId, clientInsert(common, 0, "concurrent-a"), "client-a"),
        second.applyUpdate(specId, clientInsert(common, 1, "concurrent-b"), "client-b"),
      ]);
      const waitingWriterCount = async (): Promise<number> => {
        const result = await livePool!.query<{ count: string }>(
          `SELECT count(*)
             FROM pg_stat_activity
            WHERE datname = current_database()
              AND query LIKE '%SET current_doc_seq = current_doc_seq + 1%'
              AND wait_event_type = 'Lock'`,
        );
        return Number(result.rows[0]!.count);
      };
      await waitFor(async () => (await waitingWriterCount()) === 2);
      expect(await waitingWriterCount()).toBe(2);
      await lock.query("COMMIT");
      lock.release();
      const rows = await writes;

      expect(rows.map((row) => row.seq).sort()).toEqual([2n, 3n]);
      await first.syncFromLog(specId);
      await second.syncFromLog(specId);
      expect(stateVector((await first.loadDoc(specId)).doc)).toEqual(
        stateVector((await second.loadDoc(specId)).doc),
      );
      const revisions = await livePool.query<{ seq: string }>(
        "SELECT seq FROM spec_update_log WHERE spec_id = $1 ORDER BY seq",
        [specId],
      );
      expect(revisions.rows.map((row) => BigInt(row.seq))).toEqual([1n, 2n, 3n]);

      // The published cursor has already materialized attribution through
      // seq 2. Compaction may delete that prefix, but it must retain seq 3 for
      // the next digest.
      await livePool.query(
        `INSERT INTO spec_projection
           (spec_id, rev, session_id, doc_seq, semantic_doc_seq, sha256, rendered, document_state,
            digest, digest_sha256,
            staging_path, state, requested_source, pushed_at, created_at)
         VALUES ($1, 1, $1, 2, 2, 'digest', ''::bytea, ''::bytea,
                 ''::bytea, 'digest', '/workspace/.engrams/spec/incoming-1.md',
                 'published', 'test', now(), now())`,
        [specId],
      );

      const compacted = await first.compact(specId);
      expect(compacted.coveredSeq).toBe(3n);
      const tail = await livePool.query<{ seq: string }>(
        "SELECT seq FROM spec_update_log WHERE spec_id = $1 ORDER BY seq",
        [specId],
      );
      expect(tail.rows.map((row) => BigInt(row.seq))).toEqual([3n]);
      expect(compacted.renderedMarkdown).toContain("concurrent-a");
      expect(compacted.renderedMarkdown).toContain("concurrent-b");
    },
  );
});
