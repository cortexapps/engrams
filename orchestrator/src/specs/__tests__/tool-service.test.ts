import { describe, expect, test } from "bun:test";
import {
  createTemplateDocument,
  findQuestionMarker,
  findSection,
  renderMarkdown,
  RequirementIntegrityError,
  schema,
  type SpecTemplate,
} from "@engrams/spec-document";
import { Transform } from "prosemirror-transform";

import type { SpecTemplateSection } from "../../db/schema.ts";
import {
  encodeProseMirrorDocument,
  proseMirrorDocument,
  SpecDocumentService,
  type CompactSnapshotInput,
  type SpecDocumentCheckpoint,
  type SpecDocumentStore,
  type SpecSnapshotRecord,
  type SpecUpdateEffects,
  type SpecUpdateRecord,
} from "../doc-service.ts";
import {
  OpenQuestionService,
  type CreateOpenQuestionInput,
  type OpenQuestionRecord,
  type OpenQuestionStore,
  type ResolveOpenQuestionInput,
} from "../open-questions.ts";
import { SpecQuestionDocument } from "../question-document.ts";
import {
  SectionStateService,
  type PersistSectionStateActionInput,
  type PersistSectionStateActionResult,
  type SectionStateStore,
  type SectionStateTranscriptAction,
} from "../section-state-service.ts";
import type { SectionStateValue } from "../section-state.ts";
import { SpecToolService, stableQuestionId, type SpecToolMetadataStore } from "../tool-service.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000000135";
const SESSION_ID = "00000000-0000-4000-8000-000000000136";

const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "requirements", key: "requirements", title: "Requirements" },
  ],
};

const TEMPLATE_SECTIONS: SpecTemplateSection[] = [
  {
    key: "context",
    title: "Context",
    layerKey: "understand",
    guidance: "Explain the context.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
  {
    key: "requirements",
    title: "Requirements",
    layerKey: "define",
    guidance: "List requirements.",
    doneCriteria: [],
    required: true,
    allowNa: true,
  },
];

class MemoryDocumentStore implements SpecDocumentStore {
  seq = 0n;
  readonly updates: SpecUpdateRecord[] = [];
  readonly clientIds: Array<string | null> = [];

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
    _effects: SpecUpdateEffects,
  ): Promise<bigint | null> {
    if (expectedSeq !== this.seq) return null;
    this.seq += 1n;
    this.clientIds.push(clientId);
    this.updates.push({ seq: this.seq, update: update.slice(), clientId });
    return this.seq;
  }

  async insertCheckpointAndUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    _checkpoint: SpecDocumentCheckpoint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<bigint | null> {
    return this.insertUpdateIfLatest(specId, expectedSeq, update, clientId, effects);
  }

  async notifyUpdate(): Promise<void> {}

  async compactSnapshot(_input: CompactSnapshotInput): Promise<boolean> {
    return false;
  }

  async listen(): Promise<() => Promise<void>> {
    return async () => {};
  }
}

class MemorySectionStore implements SectionStateStore {
  readonly values = new Map<string, SectionStateValue>();
  readonly actions = new Map<string, SectionStateTranscriptAction>();
  docSeq = 1n;

  async read(_specId: string, sectionId: string): Promise<SectionStateValue> {
    return this.values.get(sectionId) ?? { state: "empty", naReason: null };
  }

  async readAction(actionId: string): Promise<SectionStateTranscriptAction | null> {
    return this.actions.get(actionId) ?? null;
  }

  async persistStateAction(
    input: PersistSectionStateActionInput,
  ): Promise<PersistSectionStateActionResult> {
    const existing = this.actions.get(input.actionId);
    if (existing) return { status: "replayed", action: existing };
    const current = await this.read(input.specId, input.sectionId);
    if (
      (input.expectedDocSeq !== undefined && input.expectedDocSeq !== this.docSeq) ||
      current.state !== input.expected.state ||
      current.naReason !== input.expected.naReason
    ) {
      return { status: "conflict" };
    }
    this.values.set(input.sectionId, input.next);
    const action: SectionStateTranscriptAction = {
      id: input.actionId,
      specId: input.specId,
      sectionId: input.sectionId,
      requestFingerprint: input.requestFingerprint,
      chip: input.chip,
      createdAt: input.at,
      deliveredAt: null,
    };
    this.actions.set(action.id, action);
    return { status: "stored", action };
  }

  async listPendingActions(limit: number): Promise<SectionStateTranscriptAction[]> {
    return [...this.actions.values()].filter((action) => !action.deliveredAt).slice(0, limit);
  }

  async markActionDelivered(actionId: string, deliveredAt: Date): Promise<boolean> {
    const action = this.actions.get(actionId);
    if (!action || action.deliveredAt) return false;
    action.deliveredAt = deliveredAt;
    return true;
  }
}

class MemoryQuestionStore implements OpenQuestionStore {
  readonly rows = new Map<string, OpenQuestionRecord>();

  async find(id: string): Promise<OpenQuestionRecord | null> {
    return this.rows.get(id) ?? null;
  }

  async countOpenBySection(): Promise<Record<string, number>> {
    return {};
  }

  async create(input: CreateOpenQuestionInput): Promise<OpenQuestionRecord> {
    const existing = this.rows.get(input.id);
    if (existing) return existing;
    const row: OpenQuestionRecord = {
      ...input,
      state: "open",
      resolutionLink: null,
      resolvedAt: null,
    };
    this.rows.set(row.id, row);
    return row;
  }

  async resolve(input: ResolveOpenQuestionInput): Promise<boolean> {
    const row = this.rows.get(input.id);
    if (!row || row.state !== input.expectedState) return false;
    this.rows.set(input.id, {
      ...row,
      state: "resolved",
      resolutionLink: input.resolutionLink,
      resolvedAt: input.resolvedAt,
    });
    return true;
  }
}

class MemoryMetadata implements SpecToolMetadataStore {
  constructor(
    private readonly stateStore: MemorySectionStore,
    private readonly editors: Array<{ userId: string; name: string }> = [
      { userId: "user-1", name: "Ari" },
      { userId: "user-2", name: "Sam" },
      { userId: "user-3", name: "Sam" },
    ],
  ) {}

  async templateSections(): Promise<readonly SpecTemplateSection[]> {
    return TEMPLATE_SECTIONS;
  }

  async sectionStates(): Promise<ReadonlyMap<string, SectionStateValue>> {
    return new Map(this.stateStore.values);
  }

  async concurrentEditorNames(_specId: string, actorUserId?: string): Promise<string[]> {
    return [
      ...new Set(
        this.editors.filter((editor) => editor.userId !== actorUserId).map((editor) => editor.name),
      ),
    ].sort();
  }
}

async function setup() {
  const documentStore = new MemoryDocumentStore();
  const documents = new SpecDocumentService(documentStore);
  await documents.applyUpdate(
    SPEC_ID,
    encodeProseMirrorDocument(createTemplateDocument(TEMPLATE)),
    "seed",
  );
  const sectionStore = new MemorySectionStore();
  const questionStore = new MemoryQuestionStore();
  const service = new SpecToolService({
    documents,
    sectionStates: new SectionStateService({
      store: sectionStore,
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    }),
    questions: new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(documents, "questions"),
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    }),
    questionStore,
    metadata: new MemoryMetadata(sectionStore),
  });
  return { service, documents, documentStore, sectionStore, questionStore };
}

function context(toolCallId: string, expectedRev?: bigint) {
  return {
    sessionId: SESSION_ID,
    toolCallId,
    actorUserId: "user-1",
    ...(expectedRev === undefined ? {} : { expectedRev }),
  };
}

describe("production spec tool service", () => {
  test("reads the live document or one validated section", async () => {
    const { service } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("write-context"),
      sectionId: "context",
      markdown: "Live content",
    });

    expect(await service.read(SPEC_ID)).toMatchObject({ rev: 2n });
    expect(await service.read(SPEC_ID, "context")).toEqual({
      specId: SPEC_ID,
      rev: 2n,
      sectionId: "context",
      markdown: "## Context\n\nLive content\n",
    });
    await expect(service.read(SPEC_ID, "missing")).rejects.toThrow("Unknown spec section");
  });

  test("replaces only the section body and uses an agent client identity", async () => {
    const { service, documents, documentStore } = await setup();
    const result = await service.updateSection(SPEC_ID, {
      ...context("replace"),
      sectionId: "context",
      markdown: "New body",
      expectedRev: 1n,
    });

    expect(result).toEqual({ applied: true, newRev: 2n, concurrentEditors: ["Sam"] });
    expect(documentStore.clientIds.at(-1)).toBe(`agent:${SESSION_ID}:replace`);
    const document = proseMirrorDocument((await documents.syncFromLog(SPEC_ID)).doc);
    expect(findSection(document, "context")?.node.attrs.templateSectionKey).toBe("context");
    expect(renderMarkdown(document)).toContain("## Context\n\nNew body");
  });

  test("reports a revision conflict without changing the document", async () => {
    const { service } = await setup();
    const result = await service.updateSection(SPEC_ID, {
      ...context("stale", 0n),
      sectionId: "context",
      markdown: "Must not land",
    });

    expect(result).toEqual({ applied: false, newRev: 1n, concurrentEditors: ["Sam"] });
    expect((await service.read(SPEC_ID, "context")).markdown).not.toContain("Must not land");
  });

  test("uses the central requirement validator", async () => {
    const { service } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("requirements-1"),
      sectionId: "requirements",
      markdown: "R1 keeps the durable record.",
    });

    await expect(
      service.updateSection(SPEC_ID, {
        ...context("requirements-2"),
        sectionId: "requirements",
        markdown: "R2 replaces the old identifier.",
      }),
    ).rejects.toBeInstanceOf(RequirementIntegrityError);
  });

  test("stores a state action for later delivery and enforces template n/a rules", async () => {
    const { service, sectionStore } = await setup();
    const drafted = await service.setSectionState(SPEC_ID, {
      ...context("draft", 1n),
      sectionId: "context",
      state: "drafted",
    });

    expect(drafted).toEqual({ applied: true, newRev: 1n, concurrentEditors: ["Sam"] });
    const action = sectionStore.actions.get(`agent-section-state:${SPEC_ID}:${SESSION_ID}:draft`);
    expect(action?.deliveredAt).toBeNull();
    expect(action?.chip.provisional).toBe(false);
    await expect(
      service.setSectionState(SPEC_ID, {
        ...context("context-na"),
        sectionId: "context",
        state: "n/a",
        reason: "Not used",
      }),
    ).rejects.toThrow("does not allow the n/a state");
  });

  test("uses one deterministic question ID for an exact replay", async () => {
    const { service, questionStore } = await setup();
    const input = {
      ...context("question"),
      sectionId: "context",
      question: "What is the retry limit?",
    };

    expect(await service.addOpenQuestion(SPEC_ID, input)).toMatchObject({
      applied: true,
      newRev: 2n,
    });
    expect(await service.addOpenQuestion(SPEC_ID, input)).toMatchObject({
      applied: true,
      newRev: 2n,
    });
    expect([...questionStore.rows.keys()]).toEqual([
      stableQuestionId(SPEC_ID, SESSION_ID, "question"),
    ]);
  });

  test("repairs a marker whose question row was not committed", async () => {
    const { service, questionStore } = await setup();
    const input = {
      ...context("repair"),
      sectionId: "context",
      question: "Who owns the retry?",
    };
    await service.addOpenQuestion(SPEC_ID, input);
    const id = stableQuestionId(SPEC_ID, SESSION_ID, "repair");
    questionStore.rows.delete(id);

    const repaired = await service.addOpenQuestion(SPEC_ID, input);
    expect(repaired).toMatchObject({ applied: true, newRev: 2n });
    expect(questionStore.rows.get(id)?.text).toBe(input.question);
  });

  test("rejects a question row whose marker is missing", async () => {
    const { service, documents, questionStore } = await setup();
    const input = {
      ...context("missing-marker"),
      sectionId: "context",
      question: "Where is the marker?",
    };
    await service.addOpenQuestion(SPEC_ID, input);
    const id = stableQuestionId(SPEC_ID, SESSION_ID, "missing-marker");
    await new SpecQuestionDocument(documents).removeQuestionMarker({
      questionId: id,
      specId: SPEC_ID,
    });

    expect(questionStore.rows.has(id)).toBe(true);
    await expect(service.addOpenQuestion(SPEC_ID, input)).rejects.toThrow("has no document marker");
  });

  test("rejects a stable question ID reused for another request", async () => {
    const { service } = await setup();
    await service.addOpenQuestion(SPEC_ID, {
      ...context("collision"),
      sectionId: "context",
      question: "First question",
    });

    await expect(
      service.addOpenQuestion(SPEC_ID, {
        ...context("collision"),
        sectionId: "context",
        question: "Different question",
      }),
    ).rejects.toMatchObject({ code: "question_key_conflict" });
  });

  test("accepts an exact resolved-question retry and rejects the wrong section", async () => {
    const { service, questionStore } = await setup();
    await service.addOpenQuestion(SPEC_ID, {
      ...context("resolve-source"),
      sectionId: "context",
      question: "What is the answer?",
    });
    const questionId = [...questionStore.rows.keys()][0]!;
    const input = {
      ...context("resolve"),
      sectionId: "context",
      questionId,
      answerMarkdown: "The durable answer.",
    };

    expect(await service.resolveOpenQuestion(SPEC_ID, input)).toMatchObject({ applied: true });
    expect(await service.resolveOpenQuestion(SPEC_ID, input)).toMatchObject({ applied: true });
    await expect(
      service.resolveOpenQuestion(SPEC_ID, { ...input, sectionId: "requirements" }),
    ).rejects.toThrow("different spec section");
  });

  test("updates one diagram block only in its requested section", async () => {
    const { service, documents } = await setup();
    await documents.mutateDocument(SPEC_ID, "seed-diagram", (document) => {
      const section = findSection(document, "context");
      if (!section) throw new Error("The context section is missing.");
      return new Transform(document).insert(
        section.position + section.node.nodeSize - 1,
        schema.nodes.diagramBlock!.create({
          id: "flow",
          kind: "mermaid",
          source: "old",
          cachedRender: { kind: "mermaid", source: "old", svg: "<svg />" },
        }),
      ).doc;
    });

    const result = await service.updateBlock(SPEC_ID, {
      ...context("block"),
      sectionId: "context",
      blockId: "flow",
      source: "new",
    });
    expect(result).toMatchObject({ applied: true, newRev: 3n });
    const updated = proseMirrorDocument((await documents.syncFromLog(SPEC_ID)).doc);
    const block = findSection(updated, "context")?.node.lastChild;
    expect(block?.attrs).toMatchObject({ id: "flow", source: "new", cachedRender: null });
    await expect(
      service.updateBlock(SPEC_ID, {
        ...context("wrong-block"),
        sectionId: "requirements",
        blockId: "flow",
        source: "wrong",
      }),
    ).rejects.toThrow("different section");
  });

  test("reports later-epic notes and ticket stores as unavailable", async () => {
    const { service } = await setup();
    await expect(
      service.updateNotes(SPEC_ID, { ...context("notes"), markdown: "notes" }),
    ).rejects.toThrow("#1120");
    await expect(
      service.proposeTickets(SPEC_ID, {
        ...context("tickets"),
        idempotencyKey: "proposal-1",
        tickets: [],
      }),
    ).rejects.toThrow("#1127");
  });

  test("keeps repaired markers unresolved", async () => {
    const { service, documents, questionStore } = await setup();
    const input = {
      ...context("marker-state"),
      sectionId: "context",
      question: "Is the marker open?",
    };
    await service.addOpenQuestion(SPEC_ID, input);
    const id = stableQuestionId(SPEC_ID, SESSION_ID, "marker-state");
    questionStore.rows.delete(id);
    await service.addOpenQuestion(SPEC_ID, input);
    const marker = findQuestionMarker(
      proseMirrorDocument((await documents.syncFromLog(SPEC_ID)).doc),
      id,
      "context",
    );
    expect(marker?.node.attrs.resolved).toBe(false);
  });
});
