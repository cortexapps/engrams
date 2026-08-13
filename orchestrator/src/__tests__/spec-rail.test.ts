import {
  createTemplateDocument,
  type SectionStateTranscriptChip,
  type SpecTemplate,
} from "@engrams/spec-document";
import { afterEach, describe, expect, mock, test } from "bun:test";
import { Hono } from "hono";
import * as Y from "yjs";

import {
  makeSpecRailRoute,
  type SpecRailMetadata,
  type SpecRailStore,
} from "../routes/spec-rail.ts";
import { encodeProseMirrorDocument } from "../specs/doc-service.ts";
import type { SectionStateService } from "../specs/section-state-service.ts";
import { SectionStateReadOnlyError } from "../specs/section-state-service.ts";
import { SectionStateTransitionError } from "../specs/section-state.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001114";
const SETTLE_ACTION_ID = "00000000-0000-4000-8000-000000001117";
const UNDO_ACTION_ID = "00000000-0000-4000-8000-000000001118";
const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "design", key: "design", title: "Design" },
  ],
};
const liveDoc = new Y.Doc();
Y.applyUpdate(liveDoc, encodeProseMirrorDocument(createTemplateDocument(TEMPLATE)));

const settleChip: SectionStateTranscriptChip = {
  kind: "spec_section_state_changed",
  specId: SPEC_ID,
  sectionId: "design",
  sectionTitle: "Design",
  before: { state: "proposed", naReason: null },
  after: { state: "settled", naReason: null },
  undo: {
    kind: "restore_section_state",
    specId: SPEC_ID,
    sectionId: "design",
    expected: { state: "settled", naReason: null },
    restore: { state: "proposed", naReason: null },
  },
};

class MemoryRailStore implements SpecRailStore {
  metadata: SpecRailMetadata = {
    phase: "drafting",
    layers: [
      { key: "understand", title: "Understand" },
      { key: "define", title: "Define" },
    ],
    sections: [
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
        key: "design",
        title: "Design",
        layerKey: "define",
        guidance: "Describe the design.",
        doneCriteria: [],
        required: true,
        allowNa: true,
      },
    ],
    states: new Map([
      [
        "context",
        {
          state: "settled",
          naReason: null,
          settledBy: { id: "member-1", name: "Ada" },
          stateChangedAt: new Date("2026-08-13T08:00:00.000Z"),
        },
      ],
      [
        "design",
        {
          state: "proposed",
          naReason: null,
          settledBy: null,
          stateChangedAt: new Date("2026-08-13T09:00:00.000Z"),
        },
      ],
    ]),
    openQuestionCounts: new Map([["design", 2]]),
  };

  async readMetadata(specId: string) {
    return specId === SPEC_ID ? this.metadata : null;
  }
}

function testApp(input?: {
  store?: MemoryRailStore;
  transitionDeferred?: (
    input: Parameters<SectionStateService["transitionDeferred"]>[0],
  ) => ReturnType<SectionStateService["transitionDeferred"]>;
  undoDeferred?: (
    input: Parameters<SectionStateService["undoDeferred"]>[0],
  ) => ReturnType<SectionStateService["undoDeferred"]>;
  getSession?: () => Promise<{ user: { id: string; name: string } } | null>;
  resolveMembership?: (specId: string, userId: string) => Promise<boolean>;
}) {
  const app = new Hono();
  const store = input?.store ?? new MemoryRailStore();
  const transitionDeferred = mock(
    input?.transitionDeferred ??
      (async () => ({ value: settleChip.after, transcriptChip: settleChip })),
  );
  const undoDeferred = mock(
    input?.undoDeferred ??
      (async () => ({ value: settleChip.before, transcriptChip: settleChip })),
  );
  app.route(
    "/",
    makeSpecRailRoute({
      store,
      documents: {
        syncFromLog: async () => ({ doc: liveDoc, lastAppliedSeq: 2n, semanticDocSeq: 2n }),
      },
      sectionStates: { transitionDeferred, undoDeferred },
      resolveMembership:
        input?.resolveMembership ??
        (async (specId, userId) => specId === SPEC_ID && userId === "member-2"),
      getSession:
        input?.getSession ?? (async () => ({ user: { id: "member-2", name: "Grace" } })),
    }),
  );
  return { app, store, transitionDeferred, undoDeferred };
}

afterEach(() => mock.restore());

describe("spec rail routes", () => {
  test("returns document-ordered sections with settle credit", async () => {
    const { app } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/rail`);

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      rail: {
        sections: [
          {
            id: "context",
            templateKey: "context",
            title: "Context",
            state: "settled",
            naReason: null,
            allowNa: false,
            openQuestionCount: 0,
            settledBy: { id: "member-1", name: "Ada" },
            stateChangedAt: "2026-08-13T08:00:00.000Z",
          },
          {
            id: "design",
            templateKey: "design",
            title: "Design",
            state: "proposed",
            naReason: null,
            allowNa: true,
            openQuestionCount: 2,
            settledBy: null,
            stateChangedAt: "2026-08-13T09:00:00.000Z",
          },
        ],
        completeness: { complete: 1, total: 2 },
      },
    });
  });

  test("returns an undoable transcript chip for one-click settlement", async () => {
    const { app, transitionDeferred } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: SETTLE_ACTION_ID, state: "settled" }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ chip: settleChip });
    expect(transitionDeferred.mock.calls[0]![0]).toMatchObject({
      actionId: `rail:${SPEC_ID}:${SETTLE_ACTION_ID}`,
      target: "settled",
      actorUserId: "member-2",
      context: {
        sectionId: "design",
        sectionTitle: "Design",
        allowsNa: true,
      },
    });
  });

  test("uses the transcript chip undo payload", async () => {
    const { app, undoDeferred } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/undo`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: UNDO_ACTION_ID, undo: settleChip.undo }),
    });

    expect(response.status).toBe(200);
    expect(undoDeferred.mock.calls[0]![0]).toMatchObject({ undo: settleChip.undo });
  });

  test("rejects n/a without a reason", async () => {
    const transitionDeferred = mock(async () => {
      throw new SectionStateTransitionError(
        "na_reason_required",
        "A section in the n/a state must have a reason.",
      );
    });
    const { app } = testApp({ transitionDeferred });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: SETTLE_ACTION_ID, state: "n/a" }),
    });

    expect(response.status).toBe(400);
    expect(await response.text()).toBe("A section in the n/a state must have a reason.");
  });

  test("lets the service replay an existing action after publish", async () => {
    const store = new MemoryRailStore();
    store.metadata = { ...store.metadata, phase: "published" };
    const { app, transitionDeferred } = testApp({ store });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: SETTLE_ACTION_ID, state: "settled" }),
    });

    expect(response.status).toBe(200);
    expect(transitionDeferred).toHaveBeenCalledTimes(1);
  });

  test("maps an atomic published-state rejection to a conflict", async () => {
    const store = new MemoryRailStore();
    store.metadata = { ...store.metadata, phase: "published" };
    const transitionDeferred = mock(async () => {
      throw new SectionStateReadOnlyError();
    });
    const { app } = testApp({ store, transitionDeferred });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: SETTLE_ACTION_ID, state: "settled" }),
    });

    expect(response.status).toBe(409);
    expect(await response.text()).toBe("Only specs in drafting can change section state.");
  });

  test("does not expose an undo action to a non-member", async () => {
    const { app, undoDeferred } = testApp({
      resolveMembership: async () => false,
    });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/undo`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: UNDO_ACTION_ID, undo: settleChip.undo }),
    });

    expect(response.status).toBe(404);
    expect(undoDeferred).not.toHaveBeenCalled();
  });
});
