import { describe, expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";

import type { SpecListStore } from "../db/specs.ts";
import { SpecService } from "../gen/engram/app/v1/spec_pb.ts";
import { registerSpecs } from "../rpc/specs.ts";

const UPDATED_AT = new Date("2026-08-10T04:05:06.000Z");

function makeStore(members: string[]) {
  const listCalls: Parameters<SpecListStore["list"]>[0][] = [];
  const store: SpecListStore = {
    async isMember(userId) {
      return members.includes(userId);
    },
    async list(options) {
      listCalls.push(options);
      return {
        rows: [
          {
            id: "00000000-0000-4000-8000-000000000001",
            title: "Session durability",
            templateName: "Engineering design doc",
            repo: "cortexapps/engrams",
            lifecycle: "draft",
            participants: [{ id: "alice", name: "Alice", email: "alice@example.com" }],
            activeParticipantCount: 4,
            openQuestionCount: 2,
            ticketSyncState: "none",
            updatedAt: UPDATED_AT,
          },
        ],
        totalCount: 1,
      };
    },
  };
  return { store, listCalls };
}

function clientFor(userId: string | null, members: string[]) {
  const { store, listCalls } = makeStore(members);
  const transport = createRouterTransport((router) => {
    registerSpecs(router, {
      getSession: async () => (userId ? { user: { id: userId, role: "user" } } : null),
      store,
      orgId: "org-1",
    });
  });
  return { client: createClient(SpecService, transport), listCalls };
}

describe("SpecService", () => {
  test("the list implementation has no coordinator session verb", async () => {
    const source = await Bun.file(new URL("../rpc/specs.ts", import.meta.url)).text();
    expect(source).not.toMatch(/control-plane|SessionService|sessionsClient|coordinatorClient/);
  });

  test("lists organization specs for a member who does not own them", async () => {
    const { client, listCalls } = clientFor("bob", ["alice", "bob"]);
    const response = await client.listSpecs({ lifecycle: "draft", page: 0, pageSize: 0 });

    expect(listCalls).toEqual([{ orgId: "org-1", lifecycle: "draft", page: 1, pageSize: 200 }]);
    expect(response.totalCount).toBe(1);
    expect(response.specs[0]).toMatchObject({
      title: "Session durability",
      templateName: "Engineering design doc",
      repo: "cortexapps/engrams",
      lifecycle: "draft",
      activeParticipantCount: 4,
      openQuestionCount: 2,
      ticketSyncState: "none",
      updatedAt: UPDATED_AT.toISOString(),
    });
    expect(response.specs[0]?.participants[0]?.name).toBe("Alice");
  });

  test("returns NotFound for a non-member without reading the list", async () => {
    const { client, listCalls } = clientFor("mallory", ["alice"]);
    try {
      await client.listSpecs({});
      throw new Error("expected NotFound");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      expect((error as ConnectError).code).toBe(Code.NotFound);
    }
    expect(listCalls).toHaveLength(0);
  });
});
