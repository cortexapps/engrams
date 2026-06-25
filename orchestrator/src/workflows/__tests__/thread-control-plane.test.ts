/**
 * Real ThreadControlPlane wiring (ADR 0059 P2.7).
 *
 * The trigger-specific glue: createSession compiles the default profile (shared
 * with CreateTask), injects ENGRAM_APPEND_SYSTEM_PROMPT into harness_env so the
 * harness flag fires (the chosen delivery channel — no coordinator change), and
 * persists a `slack_thread` task owned by the resolved engrams user. Wired with
 * fakes so this logic is unit-tested without a DB or a live control plane.
 */

import { expect, test, describe } from "bun:test";
import type { ProfileRow, ProfileStore } from "../../db/profiles.ts";
import type { ImagesClient } from "../../rpc/profiles.ts";
import { makeThreadControlPlane } from "../thread-control-plane.ts";

const profileRow = (): ProfileRow => ({
  id: "default-profile",
  name: "Default",
  description: "",
  icon: "Bot",
  imageId: "img-1",
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: true,
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

describe("makeThreadControlPlane", () => {
  test("createSession injects ENGRAM_APPEND_SYSTEM_PROMPT + persists a slack_thread task", async () => {
    let createdReq: { harnessEnv?: Record<string, string> } | undefined;
    let persisted: { type: string; ownerUserId: string; sessionId: string } | undefined;

    const cp = makeThreadControlPlane({
      profiles: fakeProfiles(),
      images: fakeImages(),
      connectors: { list: async () => [] },
      secrets: { get: async () => null },
      resolveUser: async () => "user-1",
      sessions: {
        createSession: async (req) => {
          createdReq = req;
          return { sessionId: "sess-1" };
        },
        sendPrompt: async () => {},
        answerQuestion: async () => {},
        deleteSession: async () => {},
      },
      persistTask: async (args) => {
        persisted = { type: args.type, ownerUserId: args.ownerUserId, sessionId: args.sessionId };
      },
    });

    const started = await cp.createSession({
      profileId: "default-profile",
      ownerUserId: "user-1",
      prompt: "hello",
      appendSystemPrompt: "You were triggered from Slack.",
    });

    expect(createdReq?.harnessEnv?.ENGRAM_APPEND_SYSTEM_PROMPT).toBe("You were triggered from Slack.");
    expect(persisted).toEqual({ type: "slack_thread", ownerUserId: "user-1", sessionId: "sess-1" });
    expect(started.id).toBe("sess-1");
    expect(started.webUrl).toContain("/sessions/sess-1");
  });

  test("getDefaultProfile + resolveUser delegate to their seams", async () => {
    const cp = makeThreadControlPlane({
      profiles: fakeProfiles(),
      images: fakeImages(),
      connectors: { list: async () => [] },
      secrets: { get: async () => null },
      resolveUser: async (provider, ext) => (provider === "slack" && ext === "U1" ? "user-7" : null),
      sessions: {
        createSession: async () => ({ sessionId: "s" }),
        sendPrompt: async () => {},
        answerQuestion: async () => {},
        deleteSession: async () => {},
      },
      persistTask: async () => {},
    });

    expect((await cp.getDefaultProfile())?.id).toBe("default-profile");
    expect(await cp.resolveUser("slack", "U1")).toBe("user-7");
  });
});
