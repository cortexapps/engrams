import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { createTemplateDocument, renderMarkdown, type SpecTemplate } from "@engrams/spec-document";
import { Hono } from "hono";

import * as coordinatorClients from "../control-plane/client.ts";
import { makeSpecsRoute, type SpecReadRecord, type SpecReadStore } from "../routes/specs.ts";
import {
  type RestoreSectionResult,
  type SpecCheckpointRecord,
  type SpecCheckpointStore,
} from "../specs/checkpoints.ts";
import { encodeProseMirrorDocument, SpecDocumentReadOnlyError } from "../specs/doc-service.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001114";
const PINNED_ID = "00000000-0000-4000-8000-000000001115";
const CURRENT_ID = "00000000-0000-4000-8000-000000001116";
const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "design", key: "design", title: "Design" },
  ],
};
const document = createTemplateDocument(TEMPLATE);
const state = encodeProseMirrorDocument(document);

function checkpoint(id: string, label: string, docSeq: bigint): SpecCheckpointRecord {
  return {
    id,
    specId: SPEC_ID,
    state,
    stateVector: new Uint8Array(),
    renderedMarkdown: renderMarkdown(document),
    docSeq,
    label,
    authorUserId: "author-1",
    reason: "run_completed",
    createdAt: new Date(`2026-08-10T00:0${docSeq}:00.000Z`),
  };
}

class MemoryReadStore implements SpecReadStore {
  record: SpecReadRecord = {
    id: SPEC_ID,
    title: "Checkpoint-safe restore",
    lifecycle: "published",
    ownerUserId: "owner-1",
    sessionId: "session-that-must-not-boot",
    publishedCheckpointId: PINNED_ID,
    publishedAt: new Date("2026-08-10T01:00:00.000Z"),
  };

  async readSpec(specId: string): Promise<SpecReadRecord | null> {
    return specId === SPEC_ID ? this.record : null;
  }

  async listCheckpoints() {
    return [
      {
        id: CURRENT_ID,
        label: "Clarify retry boundary",
        authorUserId: "author-1",
        authorName: "Ada",
        reason: "run_completed",
        docSeq: 2n,
        createdAt: new Date("2026-08-10T00:02:00.000Z"),
      },
      {
        id: PINNED_ID,
        label: "Initial draft",
        authorUserId: "author-1",
        authorName: "Ada",
        reason: "run_completed",
        docSeq: 1n,
        createdAt: new Date("2026-08-10T00:01:00.000Z"),
      },
    ];
  }
}

class MemoryCheckpointStore implements SpecCheckpointStore {
  readonly values = new Map([
    [PINNED_ID, checkpoint(PINNED_ID, "Initial draft", 1n)],
    [CURRENT_ID, checkpoint(CURRENT_ID, "Clarify retry boundary", 2n)],
  ]);

  async insertCheckpoint(value: SpecCheckpointRecord): Promise<void> {
    this.values.set(value.id, value);
  }

  async readCheckpoint(specId: string, checkpointId: string) {
    const value = this.values.get(checkpointId) ?? null;
    return value?.specId === specId ? value : null;
  }
}

function testApp(input?: {
  readStore?: MemoryReadStore;
  checkpointStore?: MemoryCheckpointStore;
  restoreSection?: (
    specId: string,
    checkpointId: string,
    sectionId: string,
    authorUserId?: string | null,
  ) => Promise<RestoreSectionResult>;
}) {
  const app = new Hono();
  const readStore = input?.readStore ?? new MemoryReadStore();
  const checkpointStore = input?.checkpointStore ?? new MemoryCheckpointStore();
  const restoreSection = input?.restoreSection ?? mock(() => Promise.reject(new Error("unused")));
  app.route(
    "/",
    makeSpecsRoute({
      store: readStore,
      checkpointStore,
      checkpoints: { restoreSection },
      resolveMembership: async (specId, userId) => specId === SPEC_ID && userId === "member-2",
      getSession: async () => ({ user: { id: "member-2", name: "Grace" } }),
    }),
  );
  return { app, readStore, checkpointStore, restoreSection };
}

afterEach(() => mock.restore());

function spyOnEveryCoordinatorMethod() {
  const spies = Object.values(coordinatorClients).flatMap((client) => {
    const target = client as Record<string, (...args: never[]) => unknown>;
    return Object.keys(target)
      .filter((name) => typeof target[name] === "function")
      .map((name) => spyOn(target, name));
  });
  expect(spies.length).toBeGreaterThan(0);
  return spies;
}

describe("spec read routes", () => {
  test("opening a published spec reads its pinned checkpoint without a coordinator call", async () => {
    const coordinatorSpies = spyOnEveryCoordinatorMethod();
    const { app } = testApp();

    const response = await app.request(`/api/v1/specs/${SPEC_ID}`);

    expect(response.status).toBe(200);
    const body = (await response.json()) as {
      spec: {
        id: string;
        title: string;
        lifecycle: string;
        sessionId: string | null;
        publishedCheckpointId: string;
        publishedAt: string;
      };
      publishedCheckpoint: { id: string; markdown: string; sections: Array<{ id: string }> };
      checkpoints: Array<{ label: string; author: { name: string } | null }>;
    };
    expect(body.spec).toEqual({
      id: SPEC_ID,
      title: "Checkpoint-safe restore",
      lifecycle: "published",
      sessionId: null,
      publishedCheckpointId: PINNED_ID,
      publishedAt: "2026-08-10T01:00:00.000Z",
    });
    expect(body.publishedCheckpoint.id).toBe(PINNED_ID);
    expect(body.publishedCheckpoint.markdown).toContain("## Context");
    expect(body.publishedCheckpoint.sections.map((section) => section.id)).toEqual([
      "context",
      "design",
    ]);
    expect(body.checkpoints[0]).toMatchObject({
      label: "Clarify retry boundary",
      author: { name: "Ada" },
    });
    for (const coordinatorSpy of coordinatorSpies) expect(coordinatorSpy).not.toHaveBeenCalled();
  });

  test("opening a draft does not call any coordinator method", async () => {
    const coordinatorSpies = spyOnEveryCoordinatorMethod();
    const readStore = new MemoryReadStore();
    readStore.record = {
      ...readStore.record,
      lifecycle: "draft",
      publishedCheckpointId: null,
      publishedAt: null,
    };
    const { app } = testApp({ readStore });

    const response = await app.request(`/api/v1/specs/${SPEC_ID}`);

    expect(response.status).toBe(200);
    const body = (await response.json()) as {
      spec: { lifecycle: string; sessionId: string | null };
      publishedCheckpoint: unknown;
    };
    expect(body.spec).toMatchObject({ lifecycle: "draft", sessionId: null });
    expect(body.publishedCheckpoint).toBeNull();
    for (const coordinatorSpy of coordinatorSpies) expect(coordinatorSpy).not.toHaveBeenCalled();
  });

  test("a draft restore creates a checkpoint and applies a forward edit", async () => {
    const readStore = new MemoryReadStore();
    readStore.record = {
      ...readStore.record,
      lifecycle: "draft",
      publishedCheckpointId: null,
      publishedAt: null,
    };
    const beforeRestore = checkpoint(CURRENT_ID, "Before restore of context", 2n);
    const restoreSection = mock(
      async (
        specId: string,
        checkpointId: string,
        sectionId: string,
        authorUserId?: string | null,
      ) => {
        expect({ specId, checkpointId, sectionId, authorUserId }).toEqual({
          specId: SPEC_ID,
          checkpointId: PINNED_ID,
          sectionId: "context",
          authorUserId: "member-2",
        });
        return {
          applied: true as const,
          checkpointBeforeRestore: beforeRestore,
          update: {
            seq: 3n,
            semanticDocSeq: 3n,
            update: new Uint8Array(),
            clientId: `checkpoint:${PINNED_ID}`,
          },
        };
      },
    );
    const { app } = testApp({ readStore, restoreSection });

    const response = await app.request(`/api/v1/specs/${SPEC_ID}/restore`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ checkpointId: PINNED_ID, sectionId: "context" }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      applied: true,
      checkpoint: { id: CURRENT_ID, label: "Before restore of context" },
      newRev: "3",
    });
    expect(restoreSection).toHaveBeenCalledTimes(1);
  });

  test("a repeated draft restore succeeds without a checkpoint or update", async () => {
    const readStore = new MemoryReadStore();
    readStore.record = {
      ...readStore.record,
      lifecycle: "draft",
      publishedCheckpointId: null,
      publishedAt: null,
    };
    const restoreSection = mock(async (): Promise<RestoreSectionResult> => ({
      applied: false,
      checkpointBeforeRestore: null,
      update: null,
      docSeq: 3n,
    }));
    const { app } = testApp({ readStore, restoreSection });

    const response = await app.request(`/api/v1/specs/${SPEC_ID}/restore`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ checkpointId: PINNED_ID, sectionId: "context" }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ applied: false, checkpoint: null, newRev: "3" });
    expect(restoreSection).toHaveBeenCalledTimes(1);
  });

  test("a published spec refuses restore", async () => {
    const { app, restoreSection } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/restore`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ checkpointId: PINNED_ID, sectionId: "context" }),
    });

    expect(response.status).toBe(409);
    expect(restoreSection).not.toHaveBeenCalled();
  });

  test("a restore that races with publish returns a conflict", async () => {
    const readStore = new MemoryReadStore();
    readStore.record = {
      ...readStore.record,
      lifecycle: "draft",
      publishedCheckpointId: null,
      publishedAt: null,
    };
    const restoreSection = mock(async () => {
      throw new SpecDocumentReadOnlyError(SPEC_ID);
    });
    const { app } = testApp({ readStore, restoreSection });

    const response = await app.request(`/api/v1/specs/${SPEC_ID}/restore`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ checkpointId: PINNED_ID, sectionId: "context" }),
    });

    expect(response.status).toBe(409);
    expect(restoreSection).toHaveBeenCalledTimes(1);
  });
});
