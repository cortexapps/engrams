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
const CONFIRM_ACTION_ID = "00000000-0000-4000-8000-000000001117";
const UNDO_ACTION_ID = "00000000-0000-4000-8000-000000001118";
const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "design", key: "design", title: "Design" },
  ],
};
const liveDoc = new Y.Doc();
Y.applyUpdate(liveDoc, encodeProseMirrorDocument(createTemplateDocument(TEMPLATE)));

const confirmChip: SectionStateTranscriptChip = {
  kind: "spec_section_state_changed",
  specId: SPEC_ID,
  sectionId: "design",
  sectionTitle: "Design",
  before: { state: "drafted", naReason: null },
  after: { state: "confirmed", naReason: null },
  provisional: false,
  undo: {
    kind: "restore_section_state",
    specId: SPEC_ID,
    sectionId: "design",
    expected: { state: "confirmed", naReason: null },
    restore: { state: "drafted", naReason: null },
  },
};

class MemoryRailStore implements SpecRailStore {
  metadata: SpecRailMetadata = {
    lifecycle: "draft",
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
      ["context", { state: "confirmed", naReason: null }],
      ["design", { state: "drafted", naReason: null }],
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
      (async () => ({ value: confirmChip.after, transcriptChip: confirmChip })),
  );
  const undoDeferred = mock(
    input?.undoDeferred ??
      (async () => ({ value: confirmChip.before, transcriptChip: confirmChip })),
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
  test("groups sections by layer and marks the first incomplete section", async () => {
    const { app } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/rail`);

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      rail: {
        completeness: { complete: 1, total: 2 },
        frontierSectionId: "design",
        layers: [
          { title: "Understand", sections: [{ id: "context", openQuestionCount: 0 }] },
          { title: "Define", sections: [{ id: "design", openQuestionCount: 2 }] },
        ],
      },
    });
  });

  test("marks drafted downstream content as provisional", async () => {
    const store = new MemoryRailStore();
    store.metadata = {
      ...store.metadata,
      states: new Map([
        ["context", { state: "drafted", naReason: null }],
        ["design", { state: "drafted", naReason: null }],
      ]),
    };
    const { app } = testApp({ store });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/rail`);

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      rail: {
        frontierSectionId: "context",
        layers: [
          { sections: [{ id: "context", provisional: false, frontier: true }] },
          { sections: [{ id: "design", provisional: true, frontier: false }] },
        ],
      },
    });
  });

  test("returns an undoable transcript chip for one-click confirmation", async () => {
    const { app, transitionDeferred } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: CONFIRM_ACTION_ID, state: "confirmed" }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ chip: confirmChip });
    expect(transitionDeferred.mock.calls[0]![0]).toMatchObject({
      actionId: `rail:${SPEC_ID}:${CONFIRM_ACTION_ID}`,
      target: "confirmed",
      actorUserId: "member-2",
      context: {
        sectionId: "design",
        sectionTitle: "Design",
        allowsNa: true,
        unconfirmedUpstreamSectionIds: [],
      },
    });
  });

  test("uses the transcript chip undo payload", async () => {
    const { app, undoDeferred } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/undo`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: UNDO_ACTION_ID, undo: confirmChip.undo }),
    });

    expect(response.status).toBe(200);
    expect(undoDeferred.mock.calls[0]![0]).toMatchObject({ undo: confirmChip.undo });
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
      body: JSON.stringify({ actionId: CONFIRM_ACTION_ID, state: "n/a" }),
    });

    expect(response.status).toBe(400);
    expect(await response.text()).toBe("A section in the n/a state must have a reason.");
  });

  test("lets the service replay an existing action after publish", async () => {
    const store = new MemoryRailStore();
    store.metadata = { ...store.metadata, lifecycle: "published" };
    const { app, transitionDeferred } = testApp({ store });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: CONFIRM_ACTION_ID, state: "confirmed" }),
    });

    expect(response.status).toBe(200);
    expect(transitionDeferred).toHaveBeenCalledTimes(1);
  });

  test("maps an atomic published-state rejection to a conflict", async () => {
    const store = new MemoryRailStore();
    store.metadata = { ...store.metadata, lifecycle: "published" };
    const transitionDeferred = mock(async () => {
      throw new SectionStateReadOnlyError();
    });
    const { app } = testApp({ store, transitionDeferred });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/state`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: CONFIRM_ACTION_ID, state: "confirmed" }),
    });

    expect(response.status).toBe(409);
    expect(await response.text()).toBe("Published specs are read-only.");
  });

  test("does not expose an undo action to a non-member", async () => {
    const { app, undoDeferred } = testApp({
      resolveMembership: async () => false,
    });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/sections/design/undo`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId: UNDO_ACTION_ID, undo: confirmChip.undo }),
    });

    expect(response.status).toBe(404);
    expect(undoDeferred).not.toHaveBeenCalled();
  });
});
