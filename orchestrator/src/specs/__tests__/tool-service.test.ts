import { describe, expect, test } from "bun:test";
import {
  createSectionRelativeAnchor,
  createTemplateDocument,
  findQuestionMarker,
  findSection,
  parseSectionRelativeAnchor,
  renderMarkdown,
  resolveSectionRelativeAnchor,
  RequirementIntegrityError,
  schema,
  selectionSliceFingerprint,
  serializeSectionRelativeAnchor,
  type SpecTemplate,
  type SpecSelectionSpan,
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
  type LoadedSpecDocument,
  type SpecTrackedEditActionInput,
  type SpecTrackedEditActionRecord,
  type SpecSnapshotRecord,
  type SpecUpdateEffects,
  type SpecUpdateInsertResult,
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
  semanticSeq = 0n;
  readonly updates: SpecUpdateRecord[] = [];
  readonly clientIds: Array<string | null> = [];
  readonly actions = new Map<string, SpecTrackedEditActionRecord>();

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
    _participantEpoch?: bigint,
    transcriptAction?: SpecTrackedEditActionInput & { createdAt: Date },
  ): Promise<SpecUpdateInsertResult | null> {
    if (expectedSeq !== this.seq) return null;
    this.seq += 1n;
    if (_effects.semanticChanged) this.semanticSeq += 1n;
    this.clientIds.push(clientId);
    this.updates.push({
      seq: this.seq,
      semanticDocSeq: this.semanticSeq,
      update: update.slice(),
      clientId,
    });
    if (transcriptAction) {
      const action: SpecTrackedEditActionRecord = {
        ...transcriptAction,
        result: {
          applied: true,
          newRev: this.semanticSeq,
          concurrentEditors: transcriptAction.concurrentEditors,
          transcriptChip: transcriptAction.chip,
        },
        deliveredAt: null,
      };
      this.actions.set(action.id, action);
    }
    return { seq: this.seq, semanticDocSeq: this.semanticSeq };
  }

  async readTrackedEditAction(actionId: string): Promise<SpecTrackedEditActionRecord | null> {
    return this.actions.get(actionId) ?? null;
  }

  async insertCheckpointAndUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    _checkpoint: SpecDocumentCheckpoint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<SpecUpdateInsertResult | null> {
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

async function setup(
  options: { afterPersist?: (specId: string, seq: bigint) => void | Promise<void> } = {},
) {
  const documentStore = new MemoryDocumentStore();
  const documents = new SpecDocumentService(documentStore, {
    now: () => new Date("2026-08-09T12:00:00.000Z"),
    ...(options.afterPersist ? { afterPersist: options.afterPersist } : {}),
  });
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

async function selectedTextSpan(
  documents: SpecDocumentService,
  text: string,
  selectedText = text,
): Promise<SpecSelectionSpan> {
  const loaded = await documents.syncFromLog(SPEC_ID);
  const document = proseMirrorDocument(loaded.doc);
  let start = -1;
  document.descendants((node, position) => {
    const index = node.isText ? (node.text?.indexOf(text) ?? -1) : -1;
    if (start < 0 && index >= 0) start = position + index;
  });
  if (start < 0) throw new Error(`The selected test text is missing: ${text}`);
  return selectionRange(loaded, document, start, start + text.length, selectedText);
}

function selectionRange(
  loaded: LoadedSpecDocument,
  document: ReturnType<typeof proseMirrorDocument>,
  start: number,
  end: number,
  selectedText = document.textBetween(start, end, "\n"),
): SpecSelectionSpan {
  return {
    specId: SPEC_ID,
    sectionId: "context",
    revision: loaded.semanticDocSeq.toString(),
    startAnchor: serializeSectionRelativeAnchor(
      createSectionRelativeAnchor(loaded.doc, "context", start),
    ),
    endAnchor: serializeSectionRelativeAnchor(
      createSectionRelativeAnchor(loaded.doc, "context", end),
    ),
    selectedText,
    sliceFingerprint: selectionSliceFingerprint(document, start, end),
  };
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

  test("a scoped instruction produces a diff confined to the selected range", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-scoped"),
      sectionId: "context",
      markdown: "Keep this prefix. Retry forever. Keep this suffix.",
    });
    const loaded = await documents.syncFromLog(SPEC_ID);
    const document = proseMirrorDocument(loaded.doc);
    let start = -1;
    document.descendants((node, position) => {
      const index = node.isText ? (node.text?.indexOf("Retry forever.") ?? -1) : -1;
      if (index >= 0) start = position + index;
    });
    if (start < 0) throw new Error("The selected test text is missing.");
    const selectedText = "Retry forever.";
    const selection = {
      specId: SPEC_ID,
      sectionId: "context",
      revision: loaded.semanticDocSeq.toString(),
      startAnchor: serializeSectionRelativeAnchor(
        createSectionRelativeAnchor(loaded.doc, "context", start),
      ),
      endAnchor: serializeSectionRelativeAnchor(
        createSectionRelativeAnchor(loaded.doc, "context", start + selectedText.length),
      ),
      selectedText,
      sliceFingerprint: selectionSliceFingerprint(document, start, start + selectedText.length),
    };
    const untouchedRequirements = (await service.read(SPEC_ID, "requirements")).markdown;

    const result = await service.updateSection(SPEC_ID, {
      ...context("scoped-edit"),
      sectionId: "context",
      markdown: "Retry three times.",
      selection,
    });

    expect((await service.read(SPEC_ID, "context")).markdown).toBe(
      "## Context\n\nKeep this prefix. Retry three times. Keep this suffix.\n",
    );
    expect((await service.read(SPEC_ID, "requirements")).markdown).toBe(untouchedRequirements);
    expect(result.transcriptChip).toEqual({
      kind: "spec_tracked_edit",
      specId: SPEC_ID,
      sectionId: "context",
      before: "Retry forever.",
      after: "Retry three times.",
    });
  });

  test("an unrelated concurrent edit keeps the exact selected range valid", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-concurrent-outside"),
      sectionId: "context",
      markdown: "Keep this prefix. Retry forever. Keep this suffix.",
    });
    const selection = await selectedTextSpan(documents, "Retry forever.");

    await documents.mutateDocument(SPEC_ID, "concurrent-outside", (document) => {
      let position = -1;
      document.descendants((node, nodePosition) => {
        if (position < 0 && node.isText && node.text?.startsWith("Keep this prefix.")) {
          position = nodePosition;
        }
      });
      if (position < 0) throw new Error("The concurrent edit position is missing.");
      return new Transform(document).insert(position, schema.text("Concurrent: ")).doc;
    });

    const result = await service.updateSection(SPEC_ID, {
      ...context("concurrent-outside"),
      sectionId: "context",
      markdown: "Retry three times.",
      selection,
    });

    expect(result.applied).toBe(true);
    expect((await service.read(SPEC_ID, "context")).markdown).toBe(
      "## Context\n\nConcurrent: Keep this prefix. Retry three times. Keep this suffix.\n",
    );
  });

  test("selected text is display data and the structured fingerprint controls staleness", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-stale"),
      sectionId: "context",
      markdown: "Original selection",
    });
    const loaded = await documents.syncFromLog(SPEC_ID);
    const document = proseMirrorDocument(loaded.doc);
    let start = -1;
    document.descendants((node, position) => {
      if (node.isText && node.text === "Original selection") start = position;
    });
    if (start < 0) throw new Error("The stale test text is missing.");
    const selection = {
      specId: SPEC_ID,
      sectionId: "context",
      revision: loaded.semanticDocSeq.toString(),
      startAnchor: serializeSectionRelativeAnchor(
        createSectionRelativeAnchor(loaded.doc, "context", start),
      ),
      endAnchor: serializeSectionRelativeAnchor(
        createSectionRelativeAnchor(loaded.doc, "context", start + "Original selection".length),
      ),
      selectedText: "Different selection",
      sliceFingerprint: selectionSliceFingerprint(
        document,
        start,
        start + "Original selection".length,
      ),
    };

    const result = await service.updateSection(SPEC_ID, {
      ...context("display-text"),
      sectionId: "context",
      markdown: "Replacement",
      selection,
    });
    expect(result.transcriptChip?.before).toBe("Different selection");
    expect((await service.read(SPEC_ID, "context")).markdown).toContain("Replacement");
  });

  test("rejects a concurrent node-attribute change with unchanged text", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-attrs"),
      sectionId: "context",
      markdown: "### Original selection",
    });
    const selection = await selectedTextSpan(documents, "Original selection");
    await documents.mutateDocument(SPEC_ID, "concurrent-heading", (document) => {
      let headingPosition = -1;
      document.descendants((node, position) => {
        if (node.type === schema.nodes.heading && node.textContent === "Original selection") {
          headingPosition = position;
        }
      });
      if (headingPosition < 0) throw new Error("The selected heading is missing.");
      return new Transform(document).setNodeMarkup(headingPosition, undefined, { level: 4 }).doc;
    });

    await expect(
      service.updateSection(SPEC_ID, {
        ...context("stale-attrs"),
        sectionId: "context",
        markdown: "Replacement",
        selection,
      }),
    ).rejects.toThrow("selected structure changed");
  });

  test("rejects concurrent open-question and diagram changes inside the selected slice", async () => {
    const richNodeCases = ["open-question", "diagram"] as const;
    for (const richNodeCase of richNodeCases) {
      const { service, documents } = await setup();
      await service.updateSection(SPEC_ID, {
        ...context(`seed-${richNodeCase}`),
        sectionId: "context",
        markdown:
          richNodeCase === "open-question"
            ? "Before {{open-question:q-1}} after."
            : "Before diagram.\n\nAfter diagram.",
      });
      if (richNodeCase === "diagram") {
        await documents.mutateDocument(SPEC_ID, "seed-diagram-selection", (document) => {
          const section = findSection(document, "context");
          if (!section) throw new Error("The context section is missing.");
          const firstBlock = section.node.child(1);
          return new Transform(document).insert(
            section.position + 1 + firstBlock.nodeSize + 1,
            schema.nodes.diagramBlock!.create({
              id: "flow",
              kind: "mermaid",
              source: "old source",
            }),
          ).doc;
        });
      }
      const loaded = await documents.syncFromLog(SPEC_ID);
      const document = proseMirrorDocument(loaded.doc);
      let start = -1;
      let end = -1;
      document.descendants((node, position) => {
        if (node.isText && node.text?.startsWith("Before")) start = position;
        if (node.isText && node.text?.includes("after.")) end = position + node.nodeSize;
        if (node.isText && node.text === "After diagram.") end = position + node.nodeSize;
      });
      if (start < 0 || end <= start) throw new Error("The rich-node selection is missing.");
      const selection = selectionRange(loaded, document, start, end);

      await documents.mutateDocument(SPEC_ID, `change-${richNodeCase}`, (current) => {
        if (richNodeCase === "open-question") {
          const marker = findQuestionMarker(current, "q-1", "context");
          if (!marker) throw new Error("The open question is missing.");
          return new Transform(current).setNodeMarkup(marker.position, undefined, {
            ...marker.node.attrs,
            resolved: true,
            answerMarkdown: "Resolved concurrently.",
          }).doc;
        }
        let diagramPosition = -1;
        current.descendants((node, position) => {
          if (node.type === schema.nodes.diagramBlock && node.attrs.id === "flow") {
            diagramPosition = position;
          }
        });
        if (diagramPosition < 0) throw new Error("The diagram is missing.");
        return new Transform(current).setNodeMarkup(diagramPosition, undefined, {
          id: "flow",
          kind: "mermaid",
          source: "new source",
        }).doc;
      });

      await expect(
        service.updateSection(SPEC_ID, {
          ...context(`stale-${richNodeCase}`),
          sectionId: "context",
          markdown: "Replacement",
          selection,
        }),
      ).rejects.toThrow("selected structure changed");
    }
  });

  test("rejects a concurrent block-boundary change", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-boundary"),
      sectionId: "context",
      markdown: "First half second half.",
    });
    const selection = await selectedTextSpan(documents, "second half");
    await documents.mutateDocument(SPEC_ID, "split-boundary", (document) => {
      let splitPosition = -1;
      document.descendants((node, position) => {
        if (node.isText && node.text === "First half second half.") {
          splitPosition = position + "First half ".length;
        }
      });
      if (splitPosition < 0) throw new Error("The block split position is missing.");
      return new Transform(document).split(splitPosition).doc;
    });

    await expect(
      service.updateSection(SPEC_ID, {
        ...context("stale-boundary"),
        sectionId: "context",
        markdown: "second part",
        selection,
      }),
    ).rejects.toThrow(/selected (structure changed|range is no longer valid)/);
  });

  test("replays the stored tracked edit after a stop that follows the commit", async () => {
    let stopOnce = true;
    const { service, documents, documentStore } = await setup({
      afterPersist: (_specId, seq) => {
        if (!stopOnce || seq !== 3n) return;
        stopOnce = false;
        throw new Error("simulated stop after commit");
      },
    });
    await service.updateSection(SPEC_ID, {
      ...context("seed-stop"),
      sectionId: "context",
      markdown: "Retry forever.",
    });
    const selection = await selectedTextSpan(documents, "Retry forever.");
    const input = {
      ...context("stop-replay", 2n),
      sectionId: "context",
      markdown: "Retry three times.",
      selection,
    };

    await expect(service.updateSection(SPEC_ID, input)).rejects.toThrow(
      "simulated stop after commit",
    );
    expect(documentStore.actions).toHaveLength(1);

    const replay = await service.updateSection(SPEC_ID, input);
    expect(replay).toEqual({
      applied: true,
      newRev: 3n,
      concurrentEditors: ["Sam"],
      transcriptChip: {
        kind: "spec_tracked_edit",
        specId: SPEC_ID,
        sectionId: "context",
        before: "Retry forever.",
        after: "Retry three times.",
      },
    });
    expect((await service.read(SPEC_ID, "context")).markdown).toContain("Retry three times.");
  });

  test("replays Cut from storage after its relative anchors collapse", async () => {
    const { service, documents, documentStore } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-cut"),
      sectionId: "context",
      markdown: "Keep. Remove this. Keep.",
    });
    const selection = await selectedTextSpan(documents, "Remove this.");
    const input = {
      ...context("cut-replay"),
      sectionId: "context",
      markdown: "",
      selection,
    };

    const first = await service.updateSection(SPEC_ID, input);
    const afterCut = await documents.syncFromLog(SPEC_ID);
    const collapsedStart = resolveSectionRelativeAnchor(
      afterCut.doc,
      parseSectionRelativeAnchor(selection.startAnchor),
    );
    const collapsedEnd = resolveSectionRelativeAnchor(
      afterCut.doc,
      parseSectionRelativeAnchor(selection.endAnchor),
    );
    expect(
      collapsedStart === null || collapsedEnd === null || collapsedStart >= collapsedEnd,
    ).toBe(true);
    const replay = await service.updateSection(SPEC_ID, input);

    expect(replay).toEqual(first);
    expect(documentStore.actions).toHaveLength(1);
    expect((await service.read(SPEC_ID, "context")).markdown).toBe("## Context\n\nKeep.  Keep.\n");
  });

  test("rejects a stable tracked-edit action ID reused for a different request", async () => {
    const { service, documents } = await setup();
    await service.updateSection(SPEC_ID, {
      ...context("seed-mismatch"),
      sectionId: "context",
      markdown: "Retry forever.",
    });
    const selection = await selectedTextSpan(documents, "Retry forever.");
    await service.updateSection(SPEC_ID, {
      ...context("mismatch"),
      sectionId: "context",
      markdown: "Retry three times.",
      selection,
    });

    await expect(
      service.updateSection(SPEC_ID, {
        ...context("mismatch"),
        sectionId: "context",
        markdown: "Retry five times.",
        selection,
      }),
    ).rejects.toThrow("different request");
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
          cachedRender: {
            kind: "mermaid",
            source: "old",
            blockId: "flow",
            rendererRevision: "1",
            svg: "<svg />",
          },
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
