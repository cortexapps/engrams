import { describe, expect, test } from "bun:test";
import { schema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import type { SpecTemplateLayer, SpecTemplateSection } from "../../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../../routes/spec-rail.ts";
import type { LoadedSpecDocument } from "../doc-service.ts";
import {
  SpecPublishError,
  SpecPublishService,
  type PinOutcome,
  type SpecPublishRecord,
  type SpecPublishStore,
  type SpecPublishTarget,
  type SpecPublishWork,
} from "../publish.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000000126";
const SESSION_ID = "00000000-0000-4000-8000-000000000127";
const OWNER = "user-owner";
const OTHER_MEMBER = "user-member";
const NOW = new Date("2026-08-12T15:04:00.000Z");

const LAYERS: SpecTemplateLayer[] = [
  { key: "intent", title: "Intent" },
  { key: "contract", title: "Contract" },
];

function templateSection(
  key: string,
  title: string,
  layerKey: string,
  overrides: Partial<SpecTemplateSection> = {},
): SpecTemplateSection {
  return {
    key,
    title,
    layerKey,
    guidance: `Write ${title}.`,
    doneCriteria: [],
    required: true,
    allowNa: true,
    ...overrides,
  };
}

const TEMPLATE_SECTIONS: SpecTemplateSection[] = [
  templateSection("requirements", "Requirements", "intent"),
  templateSection("data", "Data model", "contract"),
  templateSection("notes", "Working notes", "contract", { required: false }),
];

function specSection(id: string, key: string, title: string, text: string) {
  return schema.nodes.section!.create({ id, templateSectionKey: key }, [
    schema.nodes.sectionHeading!.create(null, schema.text(title)),
    schema.nodes.paragraph!.create(null, text.length > 0 ? [schema.text(text)] : undefined),
  ]);
}

function document(): ProseMirrorNode {
  return schema.nodes.doc!.create(null, [
    specSection("sec-req", "requirements", "Requirements", "- R1: an org caps its sandboxes"),
    specSection("sec-data", "data", "Data model", "A counter row per org."),
    specSection("sec-notes", "notes", "Working notes", ""),
  ]);
}

function loaded(semanticDocSeq: bigint): LoadedSpecDocument {
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(document(), ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  return { doc: ydoc, lastAppliedSeq: semanticDocSeq, semanticDocSeq };
}

type SectionStateValue = { state: "open" | "proposed" | "settled" | "n/a"; naReason: string | null };

class MemoryRailStore implements SpecRailStore {
  constructor(readonly states: Map<string, SectionStateValue>) {}

  async readMetadata(): Promise<SpecRailMetadata | null> {
    return {
      phase: "drafting",
      layers: LAYERS,
      sections: TEMPLATE_SECTIONS,
      states: this.states,
      openQuestionCounts: new Map(),
    };
  }
}

class MemoryPublishStore implements SpecPublishStore {
  record: SpecPublishRecord | null = null;
  readonly questions: Array<{ id: string; sectionId: string; text: string }> = [];

  constructor(private target: SpecPublishTarget) {}

  setTarget(patch: Partial<SpecPublishTarget>): void {
    this.target = { ...this.target, ...patch };
  }

  async readTarget(): Promise<SpecPublishTarget | null> {
    return this.target;
  }

  async readPublish(): Promise<SpecPublishRecord | null> {
    return this.record;
  }

  async listOpenQuestions(): Promise<Array<{ id: string; sectionId: string; text: string }>> {
    return this.questions;
  }

  async insertRequest(record: SpecPublishRecord): Promise<SpecPublishRecord> {
    this.record ??= record;
    return this.record;
  }

  async claimDue(): Promise<SpecPublishWork[]> {
    return [];
  }

  async pin(): Promise<PinOutcome> {
    return { kind: "not_requested" };
  }

  async markBlocked(): Promise<boolean> {
    return false;
  }

  async resetBlocked(input: {
    requestedBy: string | null;
    requestedAt: Date;
    acknowledgedQuestionIds: readonly string[];
    gapCheckRunId: string | null;
  }): Promise<boolean> {
    if (this.record?.state !== "blocked") return false;
    this.record = {
      ...this.record,
      state: "requested",
      requestedBy: input.requestedBy,
      requestedAt: input.requestedAt,
      acknowledgedQuestionIds: [...input.acknowledgedQuestionIds],
      acknowledgedQuestionCount: input.acknowledgedQuestionIds.length,
      gapCheckRunId: input.gapCheckRunId,
      attempts: 0,
      nextAttemptAt: input.requestedAt,
      lastError: null,
    };
    return true;
  }

  async markArtifactPublished(): Promise<boolean> {
    return false;
  }

  async markComplete(): Promise<boolean> {
    return false;
  }

  async recordFailure(): Promise<void> {}
}

interface FixtureOptions {
  states?: Map<string, SectionStateValue>;
  questions?: Array<{ id: string; sectionId: string; text: string }>;
  target?: Partial<SpecPublishTarget>;
}

function fixture(options: FixtureOptions = {}) {
  const states =
    options.states ??
    new Map<string, SectionStateValue>([
      ["sec-req", { state: "settled", naReason: null }],
      ["sec-data", { state: "settled", naReason: null }],
    ]);
  const store = new MemoryPublishStore({
    specId: SPEC_ID,
    title: "Org sandbox quotas",
    phase: "drafting",
    ownerUserId: OWNER,
    sessionId: SESSION_ID,
    publishedCheckpointId: null,
    publishedAt: null,
    ...options.target,
  });
  store.questions.push(...(options.questions ?? []));
  let minted = 0;
  const service = new SpecPublishService({
    store,
    railStore: new MemoryRailStore(states),
    documents: { syncFromLog: async () => loaded(4n) },
    now: () => NOW,
    newId: () => `minted-${(minted += 1)}`,
  });
  return { service, store, states };
}

async function refusal(promise: Promise<unknown>): Promise<SpecPublishError> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof SpecPublishError) return error;
    throw error;
  }
  throw new Error("The publish was expected to be refused.");
}

function publishInput(overrides: Partial<Parameters<SpecPublishService["requestPublish"]>[0]> = {}) {
  return {
    specId: SPEC_ID,
    actorUserId: OWNER,
    actionId: "00000000-0000-4000-8000-0000000000aa",
    acknowledgeOpenQuestions: false,
    ...overrides,
  };
}

describe("publish confirmation service", () => {
  test("ideation refuses publish until a person starts drafting", async () => {
    const { service, store } = fixture({ target: { phase: "ideation" } });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("ideation");
    expect(error.message).toContain("Start drafting");
    expect(error.status?.phase).toBe("ideation");
    expect(error.status?.canPublish).toBe(false);
    expect(store.record).toBeNull();
  });

  test("an unsettled required section no longer blocks publish", async () => {
    const { service, store } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "proposed", naReason: null }],
      ]),
    });

    const result = await service.requestPublish(publishInput());

    expect(result.created).toBe(true);
    expect(result.publish.state).toBe("requested");
    expect(store.record).not.toBeNull();
  });

  test("n/a with a reason settles a required section (R34)", async () => {
    const { service, store } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "n/a", naReason: "The feature stores nothing." }],
      ]),
    });

    const result = await service.requestPublish(publishInput());

    expect(result.created).toBe(true);
    expect(result.publish.state).toBe("requested");
    expect(store.record?.checkpointId).toBe("minted-1");
    expect(store.record?.artifactId).toBe("minted-2");
  });

  test("section n/a state and reason do not gate publish", async () => {
    const { service } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "n/a", naReason: null }],
      ]),
    });

    const result = await service.requestPublish(publishInput());

    expect(result.created).toBe(true);
    expect(result.publish.state).toBe("requested");
  });

  test("open questions need the acknowledgment, and then they publish (R35)", async () => {
    const { service, store } = fixture({
      questions: [
        { id: "q-1", sectionId: "sec-data", text: "Do banked credits survive a downgrade?" },
        { id: "q-2", sectionId: "sec-data", text: "Who signs off on revenue over availability?" },
      ],
    });

    const error = await refusal(service.requestPublish(publishInput()));
    expect(error.code).toBe("acknowledgment_required");
    expect(error.message).toBe("2 open questions need an acknowledgment.");
    expect(error.status?.openQuestions).toEqual([
      {
        id: "q-1",
        sectionId: "sec-data",
        sectionTitle: "Data model",
        text: "Do banked credits survive a downgrade?",
      },
      {
        id: "q-2",
        sectionId: "sec-data",
        sectionTitle: "Data model",
        text: "Who signs off on revenue over availability?",
      },
    ]);
    expect(store.record).toBeNull();

    const result = await service.requestPublish(
      publishInput({ acknowledgeOpenQuestions: true }),
    );
    expect(result.created).toBe(true);
    expect(result.publish.acknowledgedQuestionCount).toBe(2);
    expect(result.publish.acknowledgedQuestionIds).toEqual(["q-1", "q-2"]);
  });

  test("publish records no gap-check run", async () => {
    const { service } = fixture();

    const result = await service.requestPublish(publishInput());

    expect(result.created).toBe(true);
    expect(result.publish.gapCheckRunId).toBeNull();
  });

  test("only the owner publishes, and a member still reads the confirmation", async () => {
    const { service, store } = fixture();

    const error = await refusal(
      service.requestPublish(publishInput({ actorUserId: OTHER_MEMBER })),
    );
    expect(error.code).toBe("not_owner");
    expect(error.status?.canPublish).toBe(false);
    expect(store.record).toBeNull();

    const status = await service.status(SPEC_ID, OTHER_MEMBER);
    expect(status.openQuestions).toEqual([]);
    expect(status.canPublish).toBe(false);
    expect((await service.status(SPEC_ID, OWNER)).canPublish).toBe(true);
  });

  test("a second request returns the first record and pins nothing new", async () => {
    const { service, store } = fixture();

    const first = await service.requestPublish(publishInput());
    const second = await service.requestPublish(
      publishInput({ actionId: "00000000-0000-4000-8000-0000000000bb" }),
    );

    expect(first.created).toBe(true);
    expect(second.created).toBe(false);
    expect(second.publish).toEqual(first.publish);
    expect(store.record?.checkpointId).toBe("minted-1");
  });

  test("a blocked publish re-gates and re-arms on the next request", async () => {
    const { service, store } = fixture({
      questions: [{ id: "q-1", sectionId: "sec-data", text: "Who signs this off?" }],
    });
    const first = await service.requestPublish(
      publishInput({ acknowledgeOpenQuestions: true }),
    );
    // The pin refused it because a new question appeared after acknowledgment.
    store.record = { ...first.publish, state: "blocked", lastError: "questions changed" };
    store.questions.push({ id: "q-2", sectionId: "sec-data", text: "And the ceiling?" });

    // The confirmation needs an acknowledgment for both current questions.
    const refused = await refusal(service.requestPublish(publishInput()));
    expect(refused.code).toBe("acknowledgment_required");
    expect(store.record?.state).toBe("blocked");

    const again = await service.requestPublish(publishInput({ acknowledgeOpenQuestions: true }));

    expect(again.created).toBe(true);
    expect(again.publish.state).toBe("requested");
    expect(again.publish.lastError).toBeNull();
    expect(again.publish.acknowledgedQuestionIds).toEqual(["q-1", "q-2"]);
    // The ids are kept: the blocked attempt created neither the checkpoint nor
    // the artifact, so reusing them keeps the publish exactly-once.
    expect(again.publish.checkpointId).toBe(first.publish.checkpointId);
    expect(again.publish.artifactId).toBe(first.publish.artifactId);
  });

  test("a blocked publish re-arms even when a section is unsettled", async () => {
    const { service, store, states } = fixture();
    const first = await service.requestPublish(publishInput());
    store.record = { ...first.publish, state: "blocked", lastError: "moved" };
    states.set("sec-data", { state: "proposed", naReason: null });

    const again = await service.requestPublish(publishInput());

    expect(again.created).toBe(true);
    expect(store.record?.state).toBe("requested");
  });

  test("a published spec with no record refuses a second publish (R38)", async () => {
    const { service } = fixture({ target: { phase: "published" } });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("already_published");
  });

  test("a spec with no drafting session cannot publish an artifact", async () => {
    const { service } = fixture({ target: { sessionId: null } });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("no_session");
  });

  test("status reports only the confirmation fields", async () => {
    const { service } = fixture();

    const status = await service.status(SPEC_ID, OWNER);

    expect(status.openQuestions).toEqual([]);
    expect(status.canPublish).toBe(true);
  });
});
