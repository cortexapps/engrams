/**
 * The spec creation flow (ADR 0114 D3, R2-R6).
 *
 * The properties under test are the ones a repeated create can break: exactly
 * one spec row, exactly one session, the template snapshot pinned at creation,
 * and a released reservation when the session never boots.
 */

import { describe, expect, mock, test } from "bun:test";
import { proseMirrorDocument } from "../specs/doc-service.ts";
import { Hono } from "hono";
import * as Y from "yjs";

import { makeSpecsRoute, type SpecReadStore } from "../routes/specs.ts";
import {
  createSpec,
  specCreateTaskParams,
  UNTITLED_SPEC,
  type CreateSpecRequest,
  type CreateSpecResult,
  type ExistingSpecRow,
  type ReservedSpecRow,
  type SpecCreateStore,
  type SpecSessionInput,
} from "../specs/create.ts";
import type { SpecTemplateSnapshot } from "../specs/template-catalog.ts";

const ORG_ID = "org-1";
const OWNER_ID = "member-2";
const TEMPLATE_ID = "00000000-0000-4000-8000-000000000115";
const OTHER_TEMPLATE_ID = "00000000-0000-4000-8000-000000000116";
const PROFILE_ID = "00000000-0000-4000-8000-0000000001a0";

const SNAPSHOT: SpecTemplateSnapshot = {
  templateId: TEMPLATE_ID,
  layers: [{ key: "intent", title: "Intent" }],
  sections: [
    {
      key: "problem",
      title: "Problem",
      layerKey: "intent",
      guidance: "State the problem.",
      doneCriteria: ["The affected user is clear."],
      required: true,
      allowNa: false,
    },
    {
      key: "design",
      title: "Design",
      layerKey: "intent",
      guidance: "Explain the design.",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
};

class MemorySpecCreateStore implements SpecCreateStore {
  readonly rows = new Map<string, ReservedSpecRow>();

  async insertIfAbsent(row: ReservedSpecRow): Promise<boolean> {
    if (this.rows.has(row.id)) return false;
    this.rows.set(row.id, row);
    return true;
  }

  async read(specId: string): Promise<ExistingSpecRow | null> {
    return this.rows.get(specId) ?? null;
  }

  async delete(specId: string): Promise<void> {
    this.rows.delete(specId);
  }
}

function testDeps(overrides?: {
  store?: MemorySpecCreateStore;
  startSession?: (input: SpecSessionInput) => Promise<void>;
  snapshot?: SpecTemplateSnapshot | null;
}) {
  const store = overrides?.store ?? new MemorySpecCreateStore();
  const seeded: Array<{ specId: string; update: Uint8Array }> = [];
  const sessions: SpecSessionInput[] = [];
  const deps = {
    store,
    catalog: {
      snapshotForNewSpec: async (orgId: string, templateId: string) => {
        if (overrides?.snapshot !== undefined) return overrides.snapshot;
        if (orgId !== ORG_ID || templateId !== TEMPLATE_ID) return null;
        return structuredClone(SNAPSHOT);
      },
    },
    documents: {
      applyUpdate: async (specId: string, update: Uint8Array) => {
        seeded.push({ specId, update });
        return {
          seq: 1n,
          semanticDocSeq: 1n,
          update,
          clientId: null,
        };
      },
    },
    startSession:
      overrides?.startSession ??
      (async (input: SpecSessionInput) => {
        sessions.push(input);
      }),
  };
  return { deps, store, seeded, sessions };
}

function request(overrides?: Partial<CreateSpecRequest>): CreateSpecRequest {
  return {
    orgId: ORG_ID,
    ownerUserId: OWNER_ID,
    idempotencyKey: "create-1",
    profileId: PROFILE_ID,
    templateId: TEMPLATE_ID,
    problemStatement: "Sessions lose their queued prompt after an eviction.",
    ...overrides,
  };
}

describe("createSpec", () => {
  test("creates one spec row and one session with the template pinned", async () => {
    const { deps, store, seeded, sessions } = testDeps();

    const result = await createSpec(deps, request());

    expect(result.created).toBe(true);
    expect(store.rows.size).toBe(1);
    expect(sessions).toHaveLength(1);

    const row = store.rows.get(result.specId)!;
    expect(row.templateId).toBe(TEMPLATE_ID);
    expect(row.ownerUserId).toBe(OWNER_ID);
    expect(row.orgId).toBe(ORG_ID);
    expect(row.sessionId).toBe(result.sessionId);
    // R4: the spec title names the session too.
    expect(row.title).toBe("Sessions lose their queued prompt after an eviction.");
    expect(sessions[0]!.title).toBe(row.title);
    expect(sessions[0]!.profileId).toBe(PROFILE_ID);
    expect(sessions[0]!.sessionId).toBe(result.sessionId);
    expect(sessions[0]!.prompt).toBe("Sessions lose their queued prompt after an eviction.");
    // The session gets the snapshot, never the live template row.
    expect(sessions[0]!.specTemplate).toEqual(SNAPSHOT);

    // The document is seeded from the template, with one section per entry.
    expect(seeded).toHaveLength(1);
    const doc = new Y.Doc();
    Y.applyUpdate(doc, seeded[0]!.update);
    const titles: string[] = [];
    proseMirrorDocument(doc).forEach((section) => {
      titles.push(section.firstChild!.textContent);
    });
    doc.destroy();
    expect(titles).toEqual(["Problem", "Design"]);
  });

  test("forwards composer overrides to the ordinary task create params", async () => {
    const { deps, sessions } = testDeps();

    await createSpec(
      deps,
      request({
        harness: "codex",
        model: "gpt-5",
        modelRouter: "",
        effort: "high",
        harnessMode: "plan",
      }),
    );

    expect(specCreateTaskParams(sessions[0]!)).toMatchObject({
      type: "spec",
      ownerUserId: OWNER_ID,
      profileId: PROFILE_ID,
      harness: "codex",
      model: "gpt-5",
      modelRouter: "",
      effort: "high",
      harnessMode: "plan",
      source: { specCreateRequestHash: sessions[0]!.requestHash },
    });
  });

  test("a second create with the same idempotency key does not double-create", async () => {
    const { deps, store, seeded, sessions } = testDeps();

    const first = await createSpec(deps, request());
    const second = await createSpec(deps, request());

    expect(second.created).toBe(false);
    expect(second.specId).toBe(first.specId);
    expect(second.sessionId).toBe(first.sessionId);
    expect(store.rows.size).toBe(1);
    expect(sessions).toHaveLength(1);
    expect(seeded).toHaveLength(1);
  });

  test("a different idempotency key creates a second spec", async () => {
    const { deps, store, sessions } = testDeps();

    const first = await createSpec(deps, request());
    const second = await createSpec(deps, request({ idempotencyKey: "create-2" }));

    expect(second.specId).not.toBe(first.specId);
    expect(second.sessionId).not.toBe(first.sessionId);
    expect(store.rows.size).toBe(2);
    expect(sessions).toHaveLength(2);
  });

  test("reusing a key with different arguments is refused", async () => {
    const { deps } = testDeps({ snapshot: structuredClone(SNAPSHOT) });

    await createSpec(deps, request());

    await expect(createSpec(deps, request({ title: "A different spec" }))).rejects.toThrow(
      /already used with different arguments/,
    );
  });

  test("reusing a key with different composer overrides is refused", async () => {
    const { deps } = testDeps({ snapshot: structuredClone(SNAPSHOT) });

    await createSpec(deps, request({ harness: "claude", effort: "high" }));

    await expect(
      createSpec(deps, request({ harness: "codex", effort: "low" })),
    ).rejects.toThrow(/already used with different arguments/);
  });

  test("an unknown template creates nothing", async () => {
    const { deps, store, sessions } = testDeps();

    await expect(createSpec(deps, request({ templateId: OTHER_TEMPLATE_ID }))).rejects.toThrow(
      /spec template not found/,
    );
    expect(store.rows.size).toBe(0);
    expect(sessions).toHaveLength(0);
  });

  test("a session that never boots releases the reservation", async () => {
    const startSession = mock(() => Promise.reject(new Error("profile not found or archived")));
    const { deps, store } = testDeps({ startSession });

    await expect(createSpec(deps, request())).rejects.toThrow(/profile not found/);
    expect(store.rows.size).toBe(0);

    // The released key starts again rather than replaying a spec with no session.
    const retry = await createSpec(testDeps({ store }).deps, request());
    expect(retry.created).toBe(true);
    expect(store.rows.size).toBe(1);
  });

  test("a service-account create reaches the session as programmatic", async () => {
    const { deps, sessions } = testDeps();

    await createSpec(deps, request({ ownerIsServiceAccount: true }));

    expect(sessions[0]!.ownerIsServiceAccount).toBe(true);
  });

  test("an ordinary create does not claim to be a service account", async () => {
    const { deps, sessions } = testDeps();

    await createSpec(deps, request());

    expect(sessions[0]!.ownerIsServiceAccount).toBeUndefined();
  });

  test("a spec with no title is named by its problem statement", async () => {
    const { deps } = testDeps();

    const result = await createSpec(deps, request({ title: "   " }));

    expect(result.title).toBe("Sessions lose their queued prompt after an eviction.");
  });

  test("a spec with neither a title nor usable prose is still named", async () => {
    const { deps } = testDeps();

    const result = await createSpec(deps, request({ problemStatement: "   " }));

    expect(result.title).toBe(UNTITLED_SPEC);
  });
});

const READ_STORE: SpecReadStore = {
  readSpec: async () => null,
  listCheckpoints: async () => [],
  renameSpec: async () => false,
};

function testApp(overrides?: {
  create?: (input: CreateSpecRequest) => Promise<CreateSpecResult>;
  role?: string;
  email?: string;
}) {
  const calls: CreateSpecRequest[] = [];
  const app = new Hono();
  app.route(
    "/",
    makeSpecsRoute({
      store: READ_STORE,
      checkpointStore: { insertCheckpoint: async () => {}, readCheckpoint: async () => null },
      checkpoints: { restoreSection: () => Promise.reject(new Error("unused")) },
      resolveMembership: async () => true,
      orgId: ORG_ID,
      create: async (input) => {
        calls.push(input);
        if (overrides?.create) return overrides.create(input);
        return {
          specId: "00000000-0000-5000-8000-0000000000aa",
          sessionId: "00000000-0000-5000-8000-0000000000bb",
          title: "Sessions lose their queued prompt after an eviction.",
          templateId: TEMPLATE_ID,
          created: true,
        };
      },
      getSession: async () => ({
        user: {
          id: OWNER_ID,
          name: "Grace",
          role: overrides?.role ?? "user",
          email: overrides?.email ?? "grace@example.com",
        },
      }),
    }),
  );
  return { app, calls };
}

function createBody(overrides?: Record<string, unknown>) {
  return {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      templateId: TEMPLATE_ID,
      profileId: PROFILE_ID,
      problemStatement: "Sessions lose their queued prompt after an eviction.",
      idempotencyKey: "create-1",
      ...overrides,
    }),
  };
}

describe("POST /api/v1/specs", () => {
  test("a member creates a spec and gets its session", async () => {
    const { app, calls } = testApp();

    const response = await app.request("/api/v1/specs", createBody());

    expect(response.status).toBe(201);
    expect(await response.json()).toEqual({
      spec: {
        id: "00000000-0000-5000-8000-0000000000aa",
        title: "Sessions lose their queued prompt after an eviction.",
        sessionId: "00000000-0000-5000-8000-0000000000bb",
        templateId: TEMPLATE_ID,
        phase: "ideation",
      },
    });
    expect(calls).toEqual([
      {
        orgId: ORG_ID,
        ownerUserId: OWNER_ID,
        templateId: TEMPLATE_ID,
        profileId: PROFILE_ID,
        problemStatement: "Sessions lose their queued prompt after an eviction.",
        idempotencyKey: "create-1",
      },
    ]);
  });

  test("forwards composer overrides from the REST body", async () => {
    const { app, calls } = testApp();

    const response = await app.request(
      "/api/v1/specs",
      createBody({
        harness: "codex",
        model: "gpt-5",
        modelRouter: "",
        effort: "high",
        harnessMode: "plan",
      }),
    );

    expect(response.status).toBe(201);
    expect(calls[0]).toMatchObject({
      harness: "codex",
      model: "gpt-5",
      modelRouter: "",
      effort: "high",
      harnessMode: "plan",
    });
  });

  test("an API-key caller is carried through as a service-account principal", async () => {
    const { app, calls } = testApp({ email: "apikey+ci-engrams@service.local" });

    const response = await app.request("/api/v1/specs", createBody());

    expect(response.status).toBe(201);
    expect(calls[0]!.ownerIsServiceAccount).toBe(true);
  });

  test("a human caller is not marked as a service account", async () => {
    const { app, calls } = testApp();

    await app.request("/api/v1/specs", createBody());

    expect(calls[0]!.ownerIsServiceAccount).toBeUndefined();
  });

  test("a replayed create answers 200, not 201", async () => {
    const { app } = testApp({
      create: async () => ({
        specId: "00000000-0000-5000-8000-0000000000aa",
        sessionId: "00000000-0000-5000-8000-0000000000bb",
        title: "Sessions lose their queued prompt after an eviction.",
        templateId: TEMPLATE_ID,
        created: false,
      }),
    });

    const response = await app.request("/api/v1/specs", createBody());

    expect(response.status).toBe(200);
  });

  test("an empty problem statement is rejected before the create path runs", async () => {
    const { app, calls } = testApp();

    const response = await app.request("/api/v1/specs", createBody({ problemStatement: "  " }));

    expect(response.status).toBe(400);
    expect(await response.text()).toContain("problemStatement");
    expect(calls).toHaveLength(0);
  });

  test.each([
    ["templateId", { templateId: "not-a-uuid" }],
    ["profileId", { profileId: "not-a-uuid" }],
    ["idempotencyKey", { idempotencyKey: "" }],
  ])("%s must be well formed", async (field, overrides) => {
    const { app, calls } = testApp();

    const response = await app.request("/api/v1/specs", createBody(overrides));

    expect(response.status).toBe(400);
    expect(await response.text()).toContain(field);
    expect(calls).toHaveLength(0);
  });

  test.each(["harness", "model", "modelRouter", "effort", "harnessMode"])(
    "rejects a non-text %s override",
    async (field) => {
      const { app, calls } = testApp();

      const response = await app.request("/api/v1/specs", createBody({ [field]: 42 }));

      expect(response.status).toBe(400);
      expect(await response.text()).toContain(field);
      expect(calls).toHaveLength(0);
    },
  );

  test("an unauthenticated caller is refused", async () => {
    const app = new Hono();
    app.route(
      "/",
      makeSpecsRoute({
        store: READ_STORE,
        checkpointStore: { insertCheckpoint: async () => {}, readCheckpoint: async () => null },
        checkpoints: { restoreSection: () => Promise.reject(new Error("unused")) },
        resolveMembership: async () => true,
        orgId: ORG_ID,
        create: () => Promise.reject(new Error("must not run")),
        getSession: async () => null,
      }),
    );

    const response = await app.request("/api/v1/specs", createBody());

    expect(response.status).toBe(401);
  });
});
