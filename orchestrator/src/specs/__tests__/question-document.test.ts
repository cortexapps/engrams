import { describe, expect, test } from "bun:test";
import {
  findQuestionMarker,
  QuestionResolutionConflictError,
  renderMarkdown,
  schema,
} from "@engrams/spec-document";

import {
  encodeProseMirrorDocument,
  proseMirrorDocument,
  SpecDocumentService,
  type CompactSnapshotInput,
  type SpecDocumentCheckpoint,
  type SpecDocumentStore,
  type SpecSnapshotRecord,
  type SpecUpdateRecord,
} from "../doc-service.ts";
import {
  OpenQuestionService,
  type OpenQuestionRecord,
  type OpenQuestionStore,
  type ResolveOpenQuestionInput,
} from "../open-questions.ts";
import { SpecQuestionDocument } from "../question-document.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000000001";
const SECTION_ID = "failure-modes";
const QUESTION_ID = "00000000-0000-4000-8000-000000000011";

class MemoryDocumentStore implements SpecDocumentStore {
  private seq = 0n;
  readonly updates: SpecUpdateRecord[] = [];

  async readSnapshot(): Promise<SpecSnapshotRecord | null> {
    return null;
  }

  async readUpdatesAfter(_specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]> {
    return this.updates.filter((row) => row.seq > afterSeq);
  }

  async insertUpdateIfLatest(
    _specId: string,
    expectedSeq: bigint,
    update: Uint8Array,
    clientId: string | null,
  ): Promise<bigint | null> {
    if (this.seq !== expectedSeq) return null;
    this.seq += 1n;
    this.updates.push({ seq: this.seq, update: update.slice(), clientId });
    return this.seq;
  }

  async insertCheckpointAndUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    _checkpoint: SpecDocumentCheckpoint,
    update: Uint8Array,
    clientId: string | null,
    _effects: Parameters<SpecDocumentStore["insertUpdateIfLatest"]>[4],
  ): Promise<bigint | null> {
    return this.insertUpdateIfLatest(specId, expectedSeq, update, clientId);
  }

  async notifyUpdate(): Promise<void> {}

  async compactSnapshot(_input: CompactSnapshotInput): Promise<boolean> {
    return true;
  }

  async listen(): Promise<() => Promise<void>> {
    return async () => {};
  }
}

class CrashOnceQuestionStore implements OpenQuestionStore {
  record: OpenQuestionRecord = {
    id: QUESTION_ID,
    specId: SPEC_ID,
    sectionId: SECTION_ID,
    text: "How many retries?",
    openedBy: "user-1",
    requestFingerprint: "question-request",
    state: "open",
    resolutionLink: null,
    resolvedAt: null,
  };
  crash = true;

  async find(): Promise<OpenQuestionRecord> {
    return { ...this.record };
  }

  async countOpenBySection(): Promise<Record<string, number>> {
    return this.record.state === "open" ? { [SECTION_ID]: 1 } : {};
  }

  async create(): Promise<OpenQuestionRecord> {
    return { ...this.record };
  }

  async resolve(input: ResolveOpenQuestionInput): Promise<boolean> {
    if (this.crash) {
      this.crash = false;
      throw new Error("The process stopped before the row update.");
    }
    if (this.record.state !== input.expectedState) return false;
    this.record = {
      ...this.record,
      state: "resolved",
      resolutionLink: input.resolutionLink,
      resolvedAt: input.resolvedAt,
    };
    return true;
  }
}

class ConcurrentQuestionStore implements OpenQuestionStore {
  record: OpenQuestionRecord = {
    id: QUESTION_ID,
    specId: SPEC_ID,
    sectionId: SECTION_ID,
    text: "How many retries?",
    openedBy: "user-1",
    requestFingerprint: "question-request",
    state: "open",
    resolutionLink: null,
    resolvedAt: null,
  };
  resolveWins = 0;
  private initialReads = 0;
  private releaseInitialReads = () => {};
  private readonly initialReadsReady = new Promise<void>((resolve) => {
    this.releaseInitialReads = resolve;
  });

  async find(): Promise<OpenQuestionRecord> {
    if (this.record.state === "open" && this.initialReads < 2) {
      this.initialReads += 1;
      if (this.initialReads === 2) this.releaseInitialReads();
      await this.initialReadsReady;
    }
    return { ...this.record };
  }

  async countOpenBySection(): Promise<Record<string, number>> {
    return this.record.state === "open" ? { [SECTION_ID]: 1 } : {};
  }

  async create(): Promise<OpenQuestionRecord> {
    return { ...this.record };
  }

  async resolve(input: ResolveOpenQuestionInput): Promise<boolean> {
    if (this.record.state !== input.expectedState) return false;
    this.resolveWins += 1;
    this.record = {
      ...this.record,
      state: "resolved",
      resolutionLink: input.resolutionLink,
      resolvedAt: input.resolvedAt,
    };
    return true;
  }
}

function unresolvedDocument() {
  const marker = schema.nodes.openQuestion!.create({
    questionId: QUESTION_ID,
    requestFingerprint: "question-request",
    resolved: false,
    answerMarkdown: null,
  });
  return schema.nodes.doc!.create(null, [
    schema.nodes.section!.create({ id: SECTION_ID, templateSectionKey: SECTION_ID }, [
      schema.nodes.sectionHeading!.create(null, schema.text("Failure modes")),
      schema.nodes.paragraph!.create(null, marker),
    ]),
  ]);
}

async function seed(store: MemoryDocumentStore): Promise<void> {
  await new SpecDocumentService(store).applyUpdate(
    SPEC_ID,
    encodeProseMirrorDocument(unresolvedDocument()),
    "seed",
  );
}

function answerOccurrences(markdown: string): number {
  return markdown.split("Use three tries.").length - 1;
}

describe("SpecQuestionDocument", () => {
  test("retries a crash after the document edit and finishes the row", async () => {
    const documentStore = new MemoryDocumentStore();
    await seed(documentStore);
    const questionStore = new CrashOnceQuestionStore();
    const service = new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(new SpecDocumentService(documentStore)),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });
    const input = { questionId: QUESTION_ID, answerMarkdown: "Use three tries." };

    await expect(service.resolve(input)).rejects.toThrow("process stopped");
    const updateCount = documentStore.updates.length;
    expect(questionStore.record.state).toBe("open");

    const retry = await service.resolve(input);
    expect(retry.state).toBe("resolved");
    expect(retry.resolutionLink).not.toBeNull();
    expect(documentStore.updates).toHaveLength(updateCount);
  });

  test("two replicas do not leave two absorbed answers", async () => {
    const store = new MemoryDocumentStore();
    await seed(store);
    const firstService = new SpecDocumentService(store);
    const secondService = new SpecDocumentService(store);
    await Promise.all([firstService.loadDoc(SPEC_ID), secondService.loadDoc(SPEC_ID)]);
    const questionStore = new ConcurrentQuestionStore();
    const first = new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(firstService, "first"),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });
    const second = new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(secondService, "second"),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const results = await Promise.all([
      first.resolve({ questionId: QUESTION_ID, answerMarkdown: "Use three tries." }),
      second.resolve({ questionId: QUESTION_ID, answerMarkdown: "Use three tries." }),
    ]);

    const merged = await new SpecDocumentService(store).loadDoc(SPEC_ID);
    const proseMirror = proseMirrorDocument(merged.doc);
    expect(results[0].resolutionLink).toBe(results[1].resolutionLink);
    expect(questionStore.resolveWins).toBe(1);
    expect(answerOccurrences(renderMarkdown(proseMirror))).toBe(1);
    expect(findQuestionMarker(proseMirror, QUESTION_ID, SECTION_ID)?.node.attrs).toMatchObject({
      resolved: true,
      answerMarkdown: "Use three tries.",
    });
  });

  test("two replicas with competing answers store only the winner", async () => {
    const store = new MemoryDocumentStore();
    await seed(store);
    const firstService = new SpecDocumentService(store);
    const secondService = new SpecDocumentService(store);
    await Promise.all([firstService.loadDoc(SPEC_ID), secondService.loadDoc(SPEC_ID)]);
    const questionStore = new ConcurrentQuestionStore();
    const first = new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(firstService, "first"),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });
    const second = new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(secondService, "second"),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const results = await Promise.allSettled([
      first.resolve({ questionId: QUESTION_ID, answerMarkdown: "Use three tries." }),
      second.resolve({ questionId: QUESTION_ID, answerMarkdown: "Do not retry." }),
    ]);

    const merged = await new SpecDocumentService(store).loadDoc(SPEC_ID);
    const proseMirror = proseMirrorDocument(merged.doc);
    const markdown = renderMarkdown(proseMirror);
    expect(results.filter(({ status }) => status === "fulfilled")).toHaveLength(1);
    const rejected = results.find(({ status }) => status === "rejected");
    expect(rejected?.status === "rejected" ? rejected.reason : null).toBeInstanceOf(
      QuestionResolutionConflictError,
    );
    expect(questionStore.resolveWins).toBe(1);
    expect(questionStore.record.resolutionLink).not.toBeNull();
    expect(answerOccurrences(markdown) + (markdown.includes("Do not retry.") ? 1 : 0)).toBe(1);
  });
});
