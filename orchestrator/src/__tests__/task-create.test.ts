/**
 * Task creation (ADR 0059 P2.7) — the shared create path.
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
  type Db,
} from "../rpc/task-create.ts";
import { CLAUDE_OAUTH_ENV_VAR } from "../db/user-secrets.ts";
import type { ProfileRow, ProfileStore } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/profiles.ts";

const profile = (over: Partial<ProfileRow> = {}): ProfileRow => ({
  id: "p1",
  name: "P",
  description: "",
  icon: "Bot",
  imageId: "img-1",
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

const deps = (token: string | null = null, images = [{ id: "img-1", imageUri: "uri-1" }]): SessionCompileDeps => ({
  images: { listEnabledImages: async () => ({ images }) } as unknown as ImagesClient,
  connectors: { list: async () => [] },
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
    expect(off.harnessEnv?.[CLAUDE_OAUTH_ENV_VAR]).toBeUndefined();
    const on = await compileSessionCreateInput(profile({ includeUserTokens: true }), deps("tok"));
    expect(on.harnessEnv?.[CLAUDE_OAUTH_ENV_VAR]).toBe("tok");
  });

  test("passes the prompt through when set", async () => {
    const inp = await compileSessionCreateInput(profile(), deps(), { prompt: "do the thing" });
    expect(inp.prompt).toBe("do the thing");
  });

  test("throws if the profile image is no longer enabled", async () => {
    await expect(compileSessionCreateInput(profile(), deps(null, []))).rejects.toThrow(/no longer enabled/);
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
