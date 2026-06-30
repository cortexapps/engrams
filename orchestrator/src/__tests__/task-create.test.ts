/**
 * Task creation (ADR 0060 P2.7) — the shared create path.
 *
 * `compileSessionCreateInput` turns a profile into a CreateSession request;
 * `createTaskWithSession` is the ONE create path (UI CreateTask RPC + the
 * external-trigger ThreadControlPlane both call it): compile → create session →
 * persist task + primary task_session, compensating the orphan session on a DB
 * failure. Pure given injected deps — unit-tested with fakes for images /
 * connectors / the user-token resolver / the upstream session client / the DB.
 */

import { expect, test, describe } from "bun:test";
import {
  compileSessionCreateInput,
  createTaskWithSession,
  type SessionCompileDeps,
  type CreateTaskDeps,
  type TaskSessionsClient,
  type HarnessCatalogClient,
  type Db,
} from "../rpc/task-create.ts";
import type { ProfileRow, ProfileStore } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/profiles.ts";

// The claude harness declares this as its `auth.user_env` (see fakeHarnessCatalog);
// the compiler injects the user token under this name (ADR 0063 — descriptor-driven).
const USER_ENV = "CLAUDE_CODE_OAUTH_TOKEN";

const profile = (over: Partial<ProfileRow> = {}): ProfileRow => ({
  id: "p1",
  name: "P",
  description: "",
  icon: "Bot",
  imageId: "img-1",
  harness: null,
  model: null,
  effort: null,
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: false,
  createdAt: new Date(0),
  updatedAt: new Date(0),
  deletedAt: null,
  ...over,
});

// A catalog with one harness ("claude"): opus default + sonnet model, high effort.
// The compiler maps the resolved model/effort id → these env vars (ADR 0063 §1).
const fakeHarnessCatalog = (): HarnessCatalogClient => ({
  listHarnesses: async () => ({
    harnesses: [
      {
        name: "claude",
        descriptor: {
          auth: { userEnv: USER_ENV },
          models: [
            { id: "opus", default: true, env: { ANTHROPIC_MODEL: "claude-opus-4-8" } },
            { id: "sonnet", default: false, env: { ANTHROPIC_MODEL: "claude-sonnet-4-6" } },
          ],
          effort: [{ id: "high", default: true, env: { MAX_THINKING_TOKENS: "32000" } }],
        },
      },
    ],
  }),
});

const deps = (token: string | null = null, images = [{ id: "img-1", imageUri: "uri-1" }]): SessionCompileDeps => ({
  images: { listEnabledImages: async () => ({ images }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
  harnessCatalog: fakeHarnessCatalog(),
  resolveUserToken: async () => token,
});

describe("compileSessionCreateInput", () => {
  test("resolves the profile image → imageUri, mode agent", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.imageUri).toBe("uri-1");
    expect(inp.mode).toBe("agent");
  });

  test("merges extraHarnessEnv last (e.g. ENGRAM_APPEND_SYSTEM_PROMPT)", async () => {
    const inp = await compileSessionCreateInput(profile({ envVars: { FOO: "bar" } }), deps(), {
      extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" },
    });
    expect(inp.harnessEnv).toMatchObject({ FOO: "bar", ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" });
  });

  test("user token injected only when includeUserTokens", async () => {
    const off = await compileSessionCreateInput(profile({ includeUserTokens: false }), deps("tok"));
    expect(off.harnessEnv?.[USER_ENV]).toBeUndefined();
    const on = await compileSessionCreateInput(profile({ includeUserTokens: true }), deps("tok"));
    expect(on.harnessEnv?.[USER_ENV]).toBe("tok");
  });

  test("injects the user token under the harness's declared user_env, not a hardcoded name", async () => {
    const customDeps: SessionCompileDeps = {
      images: {
        listEnabledImages: async () => ({ images: [{ id: "img-1", imageUri: "uri-1" }] }),
      } as unknown as ImagesClient,
      connectors: { list: async () => [] },
      harnessCatalog: {
        listHarnesses: async () => ({
          harnesses: [
            { name: "claude", descriptor: { auth: { userEnv: "OPENCODE_TOKEN" }, models: [], effort: [] } },
          ],
        }),
      },
      resolveUserToken: async (envVar) => (envVar === "OPENCODE_TOKEN" ? "tok-123" : null),
    };
    const inp = await compileSessionCreateInput(profile({ includeUserTokens: true }), customDeps);
    expect(inp.harnessEnv?.OPENCODE_TOKEN).toBe("tok-123");
    expect(inp.harnessEnv?.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
  });

  test("passes the prompt through when set", async () => {
    const inp = await compileSessionCreateInput(profile(), deps(), { prompt: "do the thing" });
    expect(inp.prompt).toBe("do the thing");
  });

  test("throws if the profile image is no longer enabled", async () => {
    await expect(compileSessionCreateInput(profile(), deps(null, []))).rejects.toThrow(/no longer enabled/);
  });

  // ADR 0063 B2: harness / model / effort resolution + env mapping.
  test("defaults to the deployment harness + descriptor default model/effort env", async () => {
    const inp = await compileSessionCreateInput(profile(), deps());
    expect(inp.harness).toBe("claude");
    expect(inp.harnessEnv).toMatchObject({
      ANTHROPIC_MODEL: "claude-opus-4-8",
      MAX_THINKING_TOKENS: "32000",
    });
  });

  test("profile default harness/model resolve when no override", async () => {
    const inp = await compileSessionCreateInput(profile({ harness: "claude", model: "sonnet" }), deps());
    expect(inp.harness).toBe("claude");
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });

  test("per-session model override beats the profile default", async () => {
    const inp = await compileSessionCreateInput(profile({ model: "opus" }), deps(), { model: "sonnet" });
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });

  test("an explicit model picker wins over a stale ANTHROPIC_MODEL in env_vars", async () => {
    const inp = await compileSessionCreateInput(
      profile({ envVars: { ANTHROPIC_MODEL: "stale" } }),
      deps(),
      { model: "sonnet" },
    );
    expect(inp.harnessEnv?.ANTHROPIC_MODEL).toBe("claude-sonnet-4-6");
  });
});

// ---------------------------------------------------------------------------
// createTaskWithSession — the shared create path
// ---------------------------------------------------------------------------

const fakeProfiles = (active = true): ProfileStore =>
  ({ getActive: async (id: string) => (active && id === "p1" ? profile() : null) }) as unknown as ProfileStore;

function fakeSessions(): TaskSessionsClient & { createReqs: unknown[]; deletedIds: string[] } {
  const createReqs: unknown[] = [];
  const deletedIds: string[] = [];
  return {
    createReqs,
    deletedIds,
    createSession: async (req) => {
      createReqs.push(req);
      return { sessionId: "sess-1" };
    },
    deleteSession: async (req) => {
      deletedIds.push(req.sessionId);
      return {};
    },
  };
}

/** A fake DB that records each `.values()` payload in insert order (task first,
 *  task_session second), or throws from the transaction when `throwOnTx`. */
function recordingDb(records: Record<string, unknown>[], throwOnTx = false): Db {
  return {
    transaction: async (fn: (tx: unknown) => Promise<unknown>) => {
      if (throwOnTx) throw new Error("db boom");
      const tx = { insert: () => ({ values: async (v: Record<string, unknown>) => void records.push(v) }) };
      return fn(tx);
    },
  } as unknown as Db;
}

const createDeps = (sessions: TaskSessionsClient, db: Db, active = true): CreateTaskDeps => ({
  profiles: fakeProfiles(active),
  images: { listEnabledImages: async () => ({ images: [{ id: "img-1", imageUri: "uri-1" }] }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
  harnessCatalog: fakeHarnessCatalog(),
  sessions,
  secrets: { get: async () => null },
  db,
});

describe("createTaskWithSession", () => {
  test("persists task + primary task_session and folds extraHarnessEnv into the session", async () => {
    const records: Record<string, unknown>[] = [];
    const sessions = fakeSessions();

    const out = await createTaskWithSession(createDeps(sessions, recordingDb(records)), {
      type: "slack_thread",
      ownerUserId: "user-1",
      profileId: "p1",
      source: { provider: "slack", team: "T1" },
      extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: "be concise" },
    });

    expect(out.sessionId).toBe("sess-1");
    expect(typeof out.taskId).toBe("string");
    expect((sessions.createReqs[0] as { harnessEnv?: Record<string, string> }).harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe(
      "be concise",
    );
    // records[0] = task, records[1] = primary task_session.
    expect(records[0]).toMatchObject({
      type: "slack_thread",
      createdByUserId: "user-1",
      status: "open",
      source: { provider: "slack", team: "T1" },
    });
    expect(records[1]).toMatchObject({ sessionId: "sess-1", role: "primary", profileId: "p1" });
  });

  test("defaults source to {} and title to null", async () => {
    const records: Record<string, unknown>[] = [];
    await createTaskWithSession(createDeps(fakeSessions(), recordingDb(records)), {
      type: "chat",
      ownerUserId: "u",
      profileId: "p1",
    });
    expect(records[0]!.source).toEqual({});
    expect(records[0]!.title).toBeNull();
  });

  test("throws NotFound for a missing/archived profile and creates no session", async () => {
    const sessions = fakeSessions();
    await expect(
      createTaskWithSession(createDeps(sessions, recordingDb([]), false), {
        type: "chat",
        ownerUserId: "u",
        profileId: "p1",
      }),
    ).rejects.toThrow(/not found or archived/);
    expect(sessions.createReqs).toHaveLength(0);
  });

  test("compensates by deleting the orphan session when the DB write fails", async () => {
    const sessions = fakeSessions();
    await expect(
      createTaskWithSession(createDeps(sessions, recordingDb([], true)), {
        type: "chat",
        ownerUserId: "u",
        profileId: "p1",
      }),
    ).rejects.toThrow(/db boom/);
    expect(sessions.deletedIds).toEqual(["sess-1"]);
  });
});
