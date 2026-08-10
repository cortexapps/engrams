import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  createTemplateDocument,
  parseMarkdown,
  renderMarkdown,
  schema,
  type SpecTemplate,
} from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";
import * as Y from "yjs";
import {
  encodeProseMirrorDocument,
  encodeSpecChannelEnvelope,
  parseSpecChannelEnvelope,
  PostgresSpecDocumentStore,
  proseMirrorDocument,
  SpecDocumentService,
  SpecDocumentTooLargeError,
  SPEC_UPDATE_SIZE_FACTOR,
  type CompactSnapshotInput,
  type SpecDocumentStore,
  type SpecSnapshotRecord,
  type SpecUpdateRecord,
} from "../doc-service.ts";
import {
  SpecCheckpointService,
  type SpecCheckpointRecord,
  type SpecCheckpointStore,
} from "../checkpoints.ts";

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
  readonly updates = new Map<string, SpecUpdateRecord[]>();
  readonly snapshots = new Map<string, SpecSnapshotRecord>();
  readonly wakes = new Set<(specId: string) => void>();
  readonly tailReads: string[] = [];
  dropNotifications = false;
  compactions = 0;

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
  ): Promise<bigint | null> {
    if ((this.currentSeq.get(specId) ?? 0n) !== expectedSeq) return null;
    const seq = (this.currentSeq.get(specId) ?? 0n) + 1n;
    this.currentSeq.set(specId, seq);
    const rows = this.updates.get(specId) ?? [];
    rows.push({ seq, update: update.slice(), clientId });
    this.updates.set(specId, rows);
    return seq;
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
      });
    }
    this.updates.set(
      input.specId,
      (this.updates.get(input.specId) ?? []).filter((row) => row.seq > input.coveredSeq),
    );
    return true;
  }

  async listen(onWake: (specId: string) => void): Promise<() => Promise<void>> {
    this.wakes.add(onWake);
    return async () => {
      this.wakes.delete(onWake);
    };
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
        blockId: "diagram-1",
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
    expect(parseSpecChannelEnvelope(`${SPEC_ID}:12`)).toBeNull();
    expect(parseSpecChannelEnvelope('{"type":"update"}')).toBeNull();
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

  test("peer notifications sync only documents in the local working set", async () => {
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

  test("restore saves a checkpoint and applies a forward section edit", async () => {
    const documentStore = new MemoryDocumentStore();
    const checkpointStore = new MemoryCheckpointStore();
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
    const markdown = renderMarkdown(proseMirrorDocument((await documents.loadDoc(SPEC_ID)).doc));
    expect(markdown).toContain("old");
    expect(markdown).not.toContain("new");
    expect(result.update.seq).toBeGreaterThan(result.checkpointBeforeRestore.docSeq);
    expect(checkpointStore.checkpoints.size).toBe(2);
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

  beforeAll(async () => {
    if (!liveDbReachable || !livePool) return;
    await livePool.query(
      `INSERT INTO spec_template
         (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Test template', '[]', '[]', '{}')`,
      [templateId],
    );
    await livePool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, 'Test spec', 'draft')`,
      [specId, templateId],
    );
  }, 15_000);

  afterAll(async () => {
    if (!livePool) return;
    if (liveDbReachable) {
      await livePool.query("DELETE FROM spec WHERE id = $1", [specId]);
      await livePool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    }
    await livePool.end();
    livePool = null;
  }, 15_000);

  test.skipIf(!liveDbReachable)(
    "two instances converge after a dropped notification by filling the log gap",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const first = new SpecDocumentService(new PostgresSpecDocumentStore(livePool));
      const second = new SpecDocumentService(new PostgresSpecDocumentStore(livePool));
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
      await livePool.query("DELETE FROM spec_update_log WHERE spec_id = $1", [specId]);
      await livePool.query("DELETE FROM spec_snapshot WHERE spec_id = $1", [specId]);
      await livePool.query("UPDATE spec SET current_doc_seq = 0 WHERE id = $1", [specId]);

      const first = new SpecDocumentService(new PostgresSpecDocumentStore(livePool));
      const second = new SpecDocumentService(new PostgresSpecDocumentStore(livePool));
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

      const compacted = await first.compact(specId);
      expect(compacted.coveredSeq).toBe(3n);
      const tail = await livePool.query<{ count: string }>(
        "SELECT count(*) FROM spec_update_log WHERE spec_id = $1",
        [specId],
      );
      expect(tail.rows[0]!.count).toBe("0");
      expect(compacted.renderedMarkdown).toContain("concurrent-a");
      expect(compacted.renderedMarkdown).toContain("concurrent-b");
    },
  );
});
