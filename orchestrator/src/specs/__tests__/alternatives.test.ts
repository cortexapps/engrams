import { describe, expect, test } from "bun:test";
import {
  createTemplateDocument,
  findSection,
  renderMarkdown,
  schema,
  SpecAlternativesError,
  type SpecAlternativeOption,
  type SpecTemplate,
} from "@engrams/spec-document";

import {
  DEFAULT_SPEC_TEMPLATE_STAGE_FLAGS,
  type SpecTemplateSection,
  type SpecTemplateStageFlags,
} from "../../db/schema.ts";
import {
  SpecAlternativesConflictError,
  SpecAlternativesService,
} from "../alternatives.ts";
import {
  encodeProseMirrorDocument,
  proseMirrorDocument,
  SpecDocumentService,
} from "../doc-service.ts";
import { OpenQuestionService } from "../open-questions.ts";
import { SpecQuestionDocument } from "../question-document.ts";
import { SectionStateService } from "../section-state-service.ts";
import type { SectionStateValue } from "../section-state.ts";
import {
  SpecAlternativesStageError,
  SpecToolService,
  type SpecToolMetadataStore,
} from "../tool-service.ts";
import { MemoryAlternativesStore } from "./memory-alternatives-store.ts";
import { MemoryDocumentStore } from "./memory-document-store.ts";
import { MemorySectionStore, MemoryQuestionStore } from "./memory-spec-stores.ts";

const SPEC_ID = "00000000-0000-4000-8000-0000000001a0";
const SESSION_ID = "00000000-0000-4000-8000-0000000001a1";
const NOW = () => new Date("2026-08-11T09:00:00.000Z");

const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "alternatives", key: "alternatives", title: "Alternatives" },
    { id: "design", key: "design", title: "Design" },
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
    key: "alternatives",
    title: "Alternatives",
    layerKey: "system",
    guidance: "Compare the serious alternatives.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
  {
    key: "design",
    title: "Design",
    layerKey: "system",
    guidance: "Explain the selected design.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
];

function option(key: string, title: string): SpecAlternativeOption {
  return {
    key,
    title,
    tradeoffs: [
      { sign: "+", text: `${key} keeps one code path` },
      { sign: "-", text: `${key} touches the gateway call sites` },
      { sign: "~", text: `${key} needs a meter later` },
    ],
  };
}

const OPTIONS = [
  option("A", "Second org-level bucket"),
  option("B", "Hierarchical limiter"),
  option("C", "Admission control at the scheduler"),
];

const COMPARISON = {
  provenance: "verified against gateway/limits.rs @ 8f2c1a4",
  rows: [
    {
      axis: "Refusal latency",
      cells: [
        { optionKey: "A", value: "gateway, under 5ms" },
        { optionKey: "B", value: "gateway, 3.1ms p99 measured" },
        { optionKey: "C", value: "post-insert, about 400ms" },
      ],
    },
  ],
};

class MemoryMetadata implements SpecToolMetadataStore {
  stageFlags: SpecTemplateStageFlags = { ...DEFAULT_SPEC_TEMPLATE_STAGE_FLAGS };

  constructor(private readonly stateStore: MemorySectionStore) {}

  async templateSections(): Promise<readonly SpecTemplateSection[]> {
    return TEMPLATE_SECTIONS;
  }

  async templateStageFlags(): Promise<SpecTemplateStageFlags> {
    return this.stageFlags;
  }

  async sectionStates(): Promise<ReadonlyMap<string, SectionStateValue>> {
    return new Map(this.stateStore.values);
  }

  async concurrentEditorNames(): Promise<string[]> {
    return [];
  }
}

async function setup() {
  const documents = new SpecDocumentService(new MemoryDocumentStore(), { now: NOW });
  await documents.applyUpdate(
    SPEC_ID,
    encodeProseMirrorDocument(createTemplateDocument(TEMPLATE)),
    "seed",
  );
  const store = new MemoryAlternativesStore();
  const alternatives = new SpecAlternativesService({ store, documents, now: NOW });
  const sectionStore = new MemorySectionStore();
  const questionStore = new MemoryQuestionStore();
  const metadata = new MemoryMetadata(sectionStore);
  const service = new SpecToolService({
    documents,
    sectionStates: new SectionStateService({ store: sectionStore, now: NOW }),
    questions: new OpenQuestionService({
      store: questionStore,
      document: new SpecQuestionDocument(documents, "questions"),
      now: NOW,
    }),
    questionStore,
    alternatives,
    metadata,
    now: NOW,
  });
  return { alternatives, documents, metadata, sectionStore, service, store };
}

async function sectionMarkdown(
  documents: SpecDocumentService,
  sectionId: string,
): Promise<string> {
  const loaded = await documents.syncFromLog(SPEC_ID);
  const section = findSection(proseMirrorDocument(loaded.doc), sectionId);
  if (!section) throw new Error(`Missing section: ${sectionId}`);
  return renderMarkdown(schema.nodes.doc!.create(null, section.node));
}

async function propose(alternatives: SpecAlternativesService, actionId = "call-1") {
  return alternatives.propose({
    specId: SPEC_ID,
    sectionId: "alternatives",
    actionId,
    options: OPTIONS,
    comparison: COMPARISON,
    leanKey: "B",
  });
}

describe("SpecAlternativesService", () => {
  test("stores a proposal and reads it back as the live stage", async () => {
    const { alternatives } = await setup();
    const chip = await propose(alternatives);

    expect(chip.setId).toBe("call-1");
    expect(chip.leanKey).toBe("B");
    const stage = await alternatives.readStage(SPEC_ID);
    expect(stage?.proposal.options.map((value) => value.key)).toEqual(["A", "B", "C"]);
    expect(stage?.decision).toBeNull();
  });

  test("picking writes every option and the winner's reason into the section", async () => {
    const { alternatives, documents } = await setup();
    await propose(alternatives);

    const result = await alternatives.decide({
      specId: SPEC_ID,
      setId: "call-1",
      optionKey: "B",
      reason: "One code path, and the team tier drops out free later.",
      decidedBy: "author",
      actionId: "call-1",
    });

    expect(result.applied).toBe(true);
    expect(result.stage.decision?.pickedKey).toBe("B");
    const markdown = await sectionMarkdown(documents, "alternatives");
    for (const candidate of OPTIONS) {
      expect(markdown).toContain(`${candidate.key} · ${candidate.title}`);
      for (const tradeoff of candidate.tradeoffs) expect(markdown).toContain(tradeoff.text);
    }
    expect(markdown).toContain("Selected: B · Hierarchical limiter");
    expect(markdown).toContain("One code path, and the team tier drops out free later.");
    expect(markdown).toContain("verified against gateway/limits.rs @ 8f2c1a4");
  });

  test("an author hybrid writes the options and the hybrid reason", async () => {
    const { alternatives, documents } = await setup();
    await propose(alternatives);

    await alternatives.decide({
      specId: SPEC_ID,
      setId: "call-1",
      optionKey: null,
      reason: "Take A's meter path with B's walk.",
      decidedBy: "author",
      actionId: "call-1",
    });

    const markdown = await sectionMarkdown(documents, "alternatives");
    expect(markdown).toContain("Selected: a hybrid");
    expect(markdown).toContain("Take A's meter path with B's walk.");
    expect(markdown).toContain("A · Second org-level bucket");
  });

  test("a replayed proposal and a replayed pick change nothing twice", async () => {
    const { alternatives, documents, store } = await setup();
    await propose(alternatives);
    await propose(alternatives);
    expect(store.rows.size).toBe(1);

    const pick = {
      specId: SPEC_ID,
      setId: "call-1",
      optionKey: "B",
      reason: "One code path.",
      decidedBy: "author" as const,
      actionId: "call-1",
    };
    const first = await alternatives.decide(pick);
    const second = await alternatives.decide(pick);

    expect(first.applied).toBe(true);
    expect(second.applied).toBe(false);
    expect(second.newRev).toBe(first.newRev);
    const markdown = await sectionMarkdown(documents, "alternatives");
    expect(markdown.match(/Selected: B/g)).toHaveLength(1);
  });

  test("a second, different pick for one set is a conflict", async () => {
    const { alternatives } = await setup();
    await propose(alternatives);
    await alternatives.decide({
      specId: SPEC_ID,
      setId: "call-1",
      optionKey: "B",
      reason: "One code path.",
      decidedBy: "author",
      actionId: "call-1",
    });

    await expect(
      alternatives.decide({
        specId: SPEC_ID,
        setId: "call-1",
        optionKey: "C",
        reason: "Second thoughts.",
        decidedBy: "author",
        actionId: "call-1",
      }),
    ).rejects.toThrow(SpecAlternativesConflictError);
  });

  test("refuses a pick for an unknown option, an empty reason, or a stale set", async () => {
    const { alternatives } = await setup();
    await propose(alternatives);
    const base = { specId: SPEC_ID, setId: "call-1", decidedBy: "author" as const };

    await expect(
      alternatives.decide({ ...base, optionKey: "Z", reason: "why", actionId: "a" }),
    ).rejects.toThrow(/unknown option: Z/);
    await expect(
      alternatives.decide({ ...base, optionKey: "B", reason: "  ", actionId: "b" }),
    ).rejects.toThrow(SpecAlternativesError);
    await expect(
      alternatives.decide({ ...base, setId: "other", optionKey: "B", reason: "why", actionId: "c" }),
    ).rejects.toThrow(SpecAlternativesConflictError);
  });

  test("refuses a set that breaks the card ceiling before it is stored", async () => {
    const { alternatives, store } = await setup();

    await expect(
      alternatives.propose({
        specId: SPEC_ID,
        sectionId: "alternatives",
        actionId: "call-1",
        options: [OPTIONS[0]!],
        comparison: {
          provenance: "checked",
          rows: [{ axis: "Blast radius", cells: [{ optionKey: "A", value: "2 files" }] }],
        },
        leanKey: null,
      }),
    ).rejects.toThrow(SpecAlternativesError);
    expect(store.rows.size).toBe(0);
  });
});

describe("layer-3 drafting waits for the pick", () => {
  const update = (service: SpecToolService, sectionId: string) =>
    service.updateSection(SPEC_ID, {
      sessionId: SESSION_ID,
      toolCallId: `call-${sectionId}`,
      sectionId,
      markdown: "Drafted body.",
    });

  test("refuses a sibling layer-3 section until the pick lands", async () => {
    const { alternatives, service } = await setup();

    await expect(update(service, "design")).rejects.toThrow(SpecAlternativesStageError);
    await propose(alternatives);
    await expect(update(service, "design")).rejects.toThrow(/waits for the alternatives pick/);

    await alternatives.decide({
      specId: SPEC_ID,
      setId: "call-1",
      optionKey: "B",
      reason: "One code path.",
      decidedBy: "author",
      actionId: "call-1",
    });
    const result = await update(service, "design");
    expect(result.applied).toBe(true);
  });

  test("never blocks the alternatives section itself or an upstream layer", async () => {
    const { service } = await setup();

    expect((await update(service, "alternatives")).applied).toBe(true);
    expect((await update(service, "context")).applied).toBe(true);
  });

  test("does not gate when the template runs the stage off", async () => {
    const { metadata, service } = await setup();
    metadata.stageFlags = { ...DEFAULT_SPEC_TEMPLATE_STAGE_FLAGS, alternatives: "off" };

    expect((await update(service, "design")).applied).toBe(true);
  });

  test("a confirmed alternatives section releases the layer without a stored set", async () => {
    const { sectionStore, service } = await setup();
    sectionStore.values.set("alternatives", { state: "confirmed", naReason: null });

    expect((await update(service, "design")).applied).toBe(true);
  });
});
