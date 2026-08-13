import { describe, expect, test } from "bun:test";
import { schema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import type { SpecTemplateLayer, SpecTemplateSection } from "../../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../../routes/spec-rail.ts";
import type { LoadedSpecDocument } from "../doc-service.ts";
import { GapCheckError } from "../gap-check.ts";
import type { GapCheckRun, GapCheckRunInput, GapCheckStatus } from "../gap-check.ts";
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
      lifecycle: "draft",
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

function gapCheckRun(id: string): GapCheckRun {
  return {
    id,
    specId: SPEC_ID,
    sessionId: SESSION_ID,
    semanticDocSeq: 4n,
    stoppedAtLayerKey: null,
    suppressedCount: 0,
    matrix: { layers: LAYERS, rows: [] },
    findings: [],
    startedBy: OWNER,
    createdAt: NOW,
  };
}

class MemoryGapCheck {
  runs: GapCheckRunInput[] = [];
  failWith: GapCheckError | null = null;

  constructor(private stale: boolean) {}

  async status(): Promise<GapCheckStatus> {
    return {
      run: this.stale ? null : gapCheckRun("run-1"),
      currentSemanticDocSeq: 4n,
      stale: this.stale,
    };
  }

  async run(input: GapCheckRunInput): Promise<GapCheckRun> {
    this.runs.push(input);
    if (this.failWith) throw this.failWith;
    this.stale = false;
    return gapCheckRun("run-2");
  }
}

interface FixtureOptions {
  states?: Map<string, SectionStateValue>;
  questions?: Array<{ id: string; sectionId: string; text: string }>;
  stale?: boolean;
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
    lifecycle: "draft",
    ownerUserId: OWNER,
    sessionId: SESSION_ID,
    publishedCheckpointId: null,
    publishedAt: null,
    gapCheckStage: "on",
    ...options.target,
  });
  store.questions.push(...(options.questions ?? []));
  const gapCheck = new MemoryGapCheck(options.stale ?? false);
  let minted = 0;
  const service = new SpecPublishService({
    store,
    railStore: new MemoryRailStore(states),
    documents: { syncFromLog: async () => loaded(4n) },
    gapCheck,
    now: () => NOW,
    newId: () => `minted-${(minted += 1)}`,
  });
  return { service, store, gapCheck, states };
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
    runGapCheck: false,
    ...overrides,
  };
}

describe("publish gate service", () => {
  test("an unsettled required section blocks the publish and names it (R34)", async () => {
    const { service, store } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "proposed", naReason: null }],
      ]),
    });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("blocked");
    expect(error.status?.gate.blockers).toEqual([
      {
        sectionId: "sec-data",
        sectionTitle: "Data model",
        layerKey: "contract",
        state: "proposed",
        reason: "proposed",
      },
    ]);
    expect(error.status?.gate.settledRequiredCount).toBe(1);
    expect(error.status?.gate.requiredCount).toBe(2);
    expect(store.record).toBeNull();
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

  test("n/a without a reason still blocks", async () => {
    const { service } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "n/a", naReason: null }],
      ]),
    });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("blocked");
    expect(error.status?.gate.blockers[0]?.reason).toBe("na_without_reason");
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
    expect(error.status?.gate.ready).toBe(true);
    expect(error.status?.gate.openQuestions).toEqual([
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

  test("a stale gap check runs as part of the publish (R30)", async () => {
    const { service, gapCheck, store } = fixture({ stale: true });

    const refused = await refusal(service.requestPublish(publishInput()));
    expect(refused.code).toBe("gap_check_stale");
    expect(gapCheck.runs).toHaveLength(0);
    expect(store.record).toBeNull();

    const result = await service.requestPublish(publishInput({ runGapCheck: true }));
    expect(gapCheck.runs).toHaveLength(1);
    expect(gapCheck.runs[0]?.requestFingerprint).toBe(
      "publish-gate:00000000-0000-4000-8000-0000000000aa",
    );
    expect(result.created).toBe(true);
  });

  test("a gap check that cannot read the document refuses plainly", async () => {
    const { service, gapCheck, store } = fixture({ stale: true });
    gapCheck.failWith = new GapCheckError(
      "untraceable_document",
      "The spec has no requirements section, so it cannot be traced.",
    );

    const error = await refusal(service.requestPublish(publishInput({ runGapCheck: true })));

    expect(error.code).toBe("gap_check_failed");
    expect(error.message).toBe("The spec has no requirements section, so it cannot be traced.");
    expect(store.record).toBeNull();
  });

  test("a template with the gap check off never asks for a run", async () => {
    const { service, gapCheck } = fixture({ stale: true, target: { gapCheckStage: "off" } });

    const result = await service.requestPublish(publishInput());

    expect(result.created).toBe(true);
    expect(gapCheck.runs).toHaveLength(0);
    expect(result.status.gapCheck.gates).toBe(false);
  });

  test("only the owner publishes, and a member still reads the gate (R37)", async () => {
    const { service, store } = fixture();

    const error = await refusal(
      service.requestPublish(publishInput({ actorUserId: OTHER_MEMBER })),
    );
    expect(error.code).toBe("not_owner");
    expect(error.status?.canPublish).toBe(false);
    expect(store.record).toBeNull();

    const status = await service.status(SPEC_ID, OTHER_MEMBER);
    expect(status.gate.ready).toBe(true);
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
    // The pin refused it: a required section moved out of the gate.
    store.record = { ...first.publish, state: "blocked", lastError: "1 required sections…" };
    store.questions.push({ id: "q-2", sectionId: "sec-data", text: "And the ceiling?" });

    // The gate still needs the acknowledgment, now for both questions.
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

  test("a blocked publish that still fails the gate stays blocked", async () => {
    const { service, store, states } = fixture();
    const first = await service.requestPublish(publishInput());
    store.record = { ...first.publish, state: "blocked", lastError: "moved" };
    states.set("sec-data", { state: "proposed", naReason: null });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("blocked");
    expect(store.record?.state).toBe("blocked");
  });

  test("verifyForPin passes for the revision it was given", async () => {
    const { service } = fixture();

    const verified = await service.verifyForPin(SPEC_ID, {
      semanticDocSeq: 4n,
      acknowledgedQuestionIds: [],
    });

    expect(verified).toEqual({ ok: true, requiredSectionIds: ["sec-req", "sec-data"] });
  });

  test("verifyForPin defers when the document moved past the compaction", async () => {
    const { service } = fixture();

    const verified = await service.verifyForPin(SPEC_ID, {
      semanticDocSeq: 3n,
      acknowledgedQuestionIds: [],
    });

    expect(verified).toEqual({
      ok: false,
      retryable: true,
      reason: "The document moved while the publish was pinning.",
    });
  });

  test("verifyForPin refuses a section that left the gate, and names it", async () => {
    const { service } = fixture({
      states: new Map([
        ["sec-req", { state: "settled", naReason: null }],
        ["sec-data", { state: "proposed", naReason: null }],
      ]),
    });

    const verified = await service.verifyForPin(SPEC_ID, {
      semanticDocSeq: 4n,
      acknowledgedQuestionIds: [],
    });

    expect(verified).toEqual({
      ok: false,
      retryable: false,
      reason: "1 required sections are no longer settled: Data model.",
    });
  });

  test("verifyForPin refuses when the open questions are not the acknowledged set", async () => {
    const { service } = fixture({
      questions: [{ id: "q-1", sectionId: "sec-data", text: "Who signs this off?" }],
    });

    const stale = await service.verifyForPin(SPEC_ID, {
      semanticDocSeq: 4n,
      acknowledgedQuestionIds: [],
    });
    expect(stale).toEqual({
      ok: false,
      retryable: false,
      reason: "The open questions changed after the acknowledgment.",
    });

    const matched = await service.verifyForPin(SPEC_ID, {
      semanticDocSeq: 4n,
      acknowledgedQuestionIds: ["q-1"],
    });
    expect(matched.ok).toBe(true);
  });

  test("a published spec with no record refuses a second publish (R38)", async () => {
    const { service } = fixture({ target: { lifecycle: "published" } });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("already_published");
  });

  test("a spec with no drafting session cannot publish an artifact", async () => {
    const { service } = fixture({ target: { sessionId: null } });

    const error = await refusal(service.requestPublish(publishInput()));

    expect(error.code).toBe("no_session");
  });

  test("an optional section never blocks, whatever its state", async () => {
    const { service } = fixture();

    const status = await service.status(SPEC_ID, OWNER);

    expect(status.gate.requiredCount).toBe(2);
    expect(status.gate.ready).toBe(true);
  });
});
