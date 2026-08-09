import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
  createTemplateDocument,
  parseMarkdown,
  renderMarkdown,
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
  private seq = 0n;
  readonly updates = new Map<string, SpecUpdateRecord[]>();
  readonly snapshots = new Map<string, SpecSnapshotRecord>();
  readonly wakes = new Set<(specId: string) => void>();
  dropNotifications = false;
  compactions = 0;

  async readSnapshot(specId: string): Promise<SpecSnapshotRecord | null> {
    return this.snapshots.get(specId) ?? null;
  }

  async readUpdatesAfter(specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]> {
    return (this.updates.get(specId) ?? []).filter((row) => row.seq > afterSeq);
  }

  async insertUpdate(
    specId: string,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<bigint> {
    this.seq += 1n;
    const rows = this.updates.get(specId) ?? [];
    rows.push({ seq: this.seq, update: update.slice(), clientId });
    this.updates.set(specId, rows);
    return this.seq;
  }

  async notifyUpdate(specId: string): Promise<void> {
    if (!this.dropNotifications) {
      for (const wake of this.wakes) wake(specId);
    }
  }

  async compactSnapshot(input: CompactSnapshotInput): Promise<boolean> {
    this.compactions += 1;
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

  async readCheckpoint(
    specId: string,
    checkpointId: string,
  ): Promise<SpecCheckpointRecord | null> {
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

  test("an update over 2 MB is rejected before persistence", async () => {
    const store = new MemoryDocumentStore();
    const service = await seededService(store);
    const doc = (await service.loadDoc(SPEC_ID)).doc;
    const update = clientInsert(
      Y.encodeStateAsUpdate(doc),
      0,
      "x".repeat(2 * 1024 * 1024 + 1),
    );

    await expect(service.applyUpdate(SPEC_ID, update, "large-client")).rejects.toBeInstanceOf(
      SpecDocumentTooLargeError,
    );
    expect(store.updates.get(SPEC_ID)).toHaveLength(1);
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
  });

  afterAll(async () => {
    if (!livePool) return;
    if (liveDbReachable) {
      await livePool.query("DELETE FROM spec WHERE id = $1", [specId]);
      await livePool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    }
    await livePool.end();
    livePool = null;
  });

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
});
