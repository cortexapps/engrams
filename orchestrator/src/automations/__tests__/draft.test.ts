import { describe, expect, test } from "bun:test";

import {
  draftAutomation,
  derivedUuid,
  emptyDraftDefinition,
  type DraftAutomationDeps,
  type DraftAutomationRequest,
  type DraftSessionInput,
} from "../draft.ts";
import type { AutomationRow, CreateAutomationInput } from "../../db/automations.ts";

function request(overrides: Partial<DraftAutomationRequest> = {}): DraftAutomationRequest {
  return {
    orgId: "org-1",
    ownerUserId: "admin-1",
    idempotencyKey: "key-1",
    prompt: "When a PR opens, run the tests and page #eng-alerts if they fail",
    profileId: "prof-1",
    ...overrides,
  };
}

interface Harness {
  deps: DraftAutomationDeps;
  created: CreateAutomationInput[];
  sessions: DraftSessionInput[];
  bound: Array<{ automationId: string; sessionId: string | null }>;
  archived: string[];
}

function harness(options: { failSession?: boolean } = {}): Harness {
  const created: CreateAutomationInput[] = [];
  const sessions: DraftSessionInput[] = [];
  const bound: Harness["bound"] = [];
  const archived: string[] = [];
  const rows = new Map<string, AutomationRow>();

  const rowFor = (input: CreateAutomationInput): AutomationRow => ({
    id: input.id!,
    name: input.name,
    description: input.description,
    enabled: input.enabled,
    kind: "user",
    builtinKey: null,
    currentVersion: 1,
    inputs: {},
    blockOverrides: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin-1",
    nextFireAt: null,
    lastFiredAt: null,
    draftSessionId: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
    archivedAt: null,
    version: {
      automationId: input.id!,
      version: 1,
      trigger: input.definition.trigger,
      blocks: input.definition.blocks,
      entrypoints: [],
      inputsSchema: input.definition.inputsSchema,
      settings: input.definition.settings,
      createdByUserId: null,
      createdAt: new Date(0),
    },
  });

  const deps: DraftAutomationDeps = {
    store: {
      async create(input) {
        if (rows.has(input.id!)) {
          const error = new Error("duplicate key") as Error & { code: string };
          error.code = "23505";
          throw error;
        }
        created.push(input);
        const row = rowFor(input);
        rows.set(row.id, row);
        return row;
      },
      async get(id) {
        return rows.get(id) ?? null;
      },
      async setDraftSession(automationId, sessionId) {
        bound.push({ automationId, sessionId });
        const row = rows.get(automationId);
        if (row) row.draftSessionId = sessionId;
      },
      async archive(id) {
        archived.push(id);
        const row = rows.get(id);
        if (row) row.archivedAt = new Date(0);
        return row ?? null;
      },
    },
    startSession: async (input) => {
      if (options.failSession) throw new Error("session boot failed");
      sessions.push(input);
    },
  };
  return { deps, created, sessions, bound, archived };
}

describe("draftAutomation", () => {
  test("creates a disabled automation, binds the derived session, boots the drafting task", async () => {
    const h = harness();
    const result = await draftAutomation(h.deps, request());
    expect(result.created).toBe(true);
    expect(h.created[0]).toMatchObject({
      id: result.automationId,
      enabled: false,
      definition: emptyDraftDefinition(),
    });
    expect(h.bound[0]).toEqual({
      automationId: result.automationId,
      sessionId: result.sessionId,
    });
    const session = h.sessions[0]!;
    expect(session.sessionId).toBe(result.sessionId);
    expect(session.automationId).toBe(result.automationId);
    expect(session.prompt).toContain("run the tests");
  });

  test("a byte-identical retry replays: same ids, created false, no second session", async () => {
    const h = harness();
    const first = await draftAutomation(h.deps, request());
    const second = await draftAutomation(h.deps, request());
    expect(second).toEqual({ ...first, created: false });
    expect(h.sessions).toHaveLength(1);
    expect(h.created).toHaveLength(1);
  });

  test("the same key with different arguments derives a fresh draft", async () => {
    const h = harness();
    const first = await draftAutomation(h.deps, request());
    const second = await draftAutomation(h.deps, request({ prompt: "Different automation" }));
    expect(second.automationId).not.toBe(first.automationId);
    expect(second.created).toBe(true);
  });

  test("a failed session boot releases the reservation and rethrows", async () => {
    const h = harness({ failSession: true });
    await expect(draftAutomation(h.deps, request())).rejects.toThrow("session boot failed");
    expect(h.archived).toHaveLength(1);
  });

  test("an empty prompt is refused before any write", async () => {
    const h = harness();
    await expect(draftAutomation(h.deps, request({ prompt: "  " }))).rejects.toThrow(
      /prompt is required/,
    );
    expect(h.created).toHaveLength(0);
  });

  test("derivedUuid is stable, uuid-shaped, and length-prefixed", () => {
    expect(derivedUuid("a", "b")).toBe(derivedUuid("a", "b"));
    expect(derivedUuid("a", "b")).not.toBe(derivedUuid("ab", ""));
    expect(derivedUuid("x")).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
  });
});
