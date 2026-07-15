/**
 * Real ThreadControlPlane wiring (ADR 0060 P2.7).
 *
 * The trigger-specific glue: createTask runs the shared create path (the same
 * one CreateTask uses) against the default profile, injects
 * ENGRAM_APPEND_SYSTEM_PROMPT into harness_env so the harness flag fires (the
 * chosen delivery channel — no coordinator change), and persists a
 * `slack_thread` task owned by the resolved engrams user. Wired with fakes so
 * this logic is unit-tested without a live control plane (a recording fake DB
 * captures the persisted task + task_session rows).
 */

import { expect, test, describe } from "bun:test";
import type { ProfileRow, ProfileStore } from "../../db/profiles.ts";
import type { ImagesClient } from "../../rpc/profiles.ts";
import type { Db, HarnessCatalogClient } from "../../rpc/task-create.ts";
import { makeThreadControlPlane } from "../thread-control-plane.ts";

const profileRow = (): ProfileRow => ({
  id: "default-profile",
  name: "Default",
  description: "",
  icon: "Bot",
  imageId: "img-1",
  harness: "claude",
  model: null,
  effort: null,
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: true,
  portExposures: [],
  createdAt: new Date(0),
  updatedAt: new Date(0),
  deletedAt: null,
});

const fakeProfiles = (): ProfileStore =>
  ({
    getActive: async (id: string) => (id === "default-profile" ? profileRow() : null),
    getDefault: async () => profileRow(),
  }) as unknown as ProfileStore;

const fakeImages = (): ImagesClient =>
  ({ listEnabledImages: async () => ({ images: [{ id: "img-1", imageUri: "uri-1" }] }) }) as unknown as ImagesClient;

const fakeHarnessCatalog = (): HarnessCatalogClient => ({
  listHarnesses: async () => ({
    harnesses: [
      {
        name: "claude",
        descriptor: {
          models: [{ id: "opus", default: true, env: { ANTHROPIC_MODEL: "claude-opus-4-8" } }],
          effort: [],
        },
      },
    ],
  }),
});

/** A fake DB that records each `.values()` payload in insert order (task first,
 *  primary task_session second). */
function recordingDb(records: Record<string, unknown>[]): Db {
  return {
    transaction: async (fn: (tx: unknown) => Promise<unknown>) => {
      const tx = { insert: () => ({ values: async (v: Record<string, unknown>) => void records.push(v) }) };
      return fn(tx);
    },
  } as unknown as Db;
}

describe("makeThreadControlPlane", () => {
  test("createTask injects ENGRAM_APPEND_SYSTEM_PROMPT + persists a slack_thread task", async () => {
    let createdReq: { harnessEnv?: Record<string, string> } | undefined;
    const records: Record<string, unknown>[] = [];

    const cp = makeThreadControlPlane({
      profiles: fakeProfiles(),
      images: fakeImages(),
      connectors: { list: async () => [] },
      harnessCatalog: fakeHarnessCatalog(),
      secrets: { get: async () => null },
      resolveUser: async () => "user-1",
      db: recordingDb(records),
      sessions: {
        createSession: async (req) => {
          createdReq = req;
          return { sessionId: "sess-1" };
        },
        sendPrompt: async () => {},
        deleteSession: async () => {},
      },
    });

    const started = await cp.createTask({
      profileId: "default-profile",
      ownerUserId: "user-1",
      prompt: "hello",
      appendSystemPrompt: "You were triggered from Slack.",
      source: { provider: "slack", team: "T1", channel: "C1", threadRoot: "100.0" },
      threadWorkflowId: "thread-wf-1",
    });

    expect(createdReq?.harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe("You were triggered from Slack.");
    // records[0] = task, records[1] = primary task_session.
    expect(records[0]).toMatchObject({
      type: "slack_thread",
      createdByUserId: "user-1",
      source: { provider: "slack", team: "T1", channel: "C1", threadRoot: "100.0" },
    });
    expect(records[1]).toMatchObject({ sessionId: "sess-1", role: "primary", profileId: "default-profile" });
    expect(records[2]).toEqual({ sessionId: "sess-1", threadWfId: "thread-wf-1" });
    expect(records[3]).toEqual({ sessionId: "sess-1" });
    expect(started.id).toBe("sess-1");
    expect(started.webUrl).toContain("/sessions/sess-1");
  });

  test("getDefaultProfile + resolveUser delegate to their seams", async () => {
    const completed: unknown[][] = [];
    const cp = makeThreadControlPlane({
      profiles: fakeProfiles(),
      images: fakeImages(),
      connectors: { list: async () => [] },
      harnessCatalog: fakeHarnessCatalog(),
      secrets: { get: async () => null },
      resolveUser: async (provider, ext) => (provider === "slack" && ext === "U1" ? "user-7" : null),
      toolRegistry: {
        complete: async (...args) => void completed.push(args),
      },
      db: recordingDb([]),
      sessions: {
        createSession: async () => ({ sessionId: "s" }),
        sendPrompt: async () => {},
        deleteSession: async () => {},
      },
    });

    expect((await cp.getDefaultProfile())?.id).toBe("default-profile");
    expect(await cp.resolveUser("slack", "U1")).toBe("user-7");
    await cp.completeToolCall("s", "tc", { "Ship?": ["Yes"] });
    expect(completed).toEqual([["s", "tc", { "Ship?": ["Yes"] }]]);
  });
});
