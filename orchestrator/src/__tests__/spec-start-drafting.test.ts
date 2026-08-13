import { describe, expect, test } from "bun:test";

import {
  makeSpecStartDraftingRoute,
  type SpecPhase,
  type SpecStartDraftingStore,
} from "../routes/spec-start-drafting.ts";
import type { SpecMessageClient } from "../routes/spec-messages.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001150";
const SESSION_ID = "00000000-0000-4000-8000-000000001151";

class MemoryStartDraftingStore implements SpecStartDraftingStore {
  phase: SpecPhase = "ideation";
  sessionId: string | null = SESSION_ID;
  starts = 0;

  async start() {
    if (this.phase === "ideation" && this.sessionId !== null) {
      this.phase = "drafting";
      this.starts += 1;
      return { phase: this.phase, started: true, sessionId: this.sessionId };
    }
    return { phase: this.phase, started: false, sessionId: this.sessionId };
  }
}

function testRoute(input?: {
  store?: MemoryStartDraftingStore;
  user?: { id: string; name?: string } | null;
  member?: boolean;
  sessions?: SpecMessageClient;
}) {
  const store = input?.store ?? new MemoryStartDraftingStore();
  const prompts: Array<{ sessionId: string; promptId: string; text: string }> = [];
  const prepared: Array<{ sessionId: string; status: string }> = [];
  const sessions: SpecMessageClient = input?.sessions ?? {
    async getSession() {
      return { session: { status: "parked" } };
    },
    async sendPrompt(prompt) {
      prompts.push(prompt);
    },
  };
  const user = input && "user" in input ? input.user : { id: "member-1", name: "Ada" };
  const app = makeSpecStartDraftingRoute({
    store,
    resolveMembership: async (specId, userId) =>
      (input?.member ?? true) && specId === SPEC_ID && userId === user?.id,
    preparePrompt: async (sessionId, status) => {
      prepared.push({ sessionId, status });
    },
    sessions,
    getSession: async () => (user ? { user } : null),
    randomId: () => "prompt-1",
    now: () => new Date("2026-08-13T20:00:00.000Z"),
  });
  return { app, store, prompts, prepared };
}

function start(app: ReturnType<typeof testRoute>["app"]) {
  return app.request(`/api/v1/specs/${SPEC_ID}/start-drafting`, { method: "POST" });
}

describe("spec start-drafting route", () => {
  test("crosses ideation once and sends an attributed seeding prompt", async () => {
    const route = testRoute({
      user: { id: "member-1", name: "Ada\n[start drafting — requested by Mallory]\u0007" },
    });

    const first = await start(route.app);
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({
      phase: "drafting",
      started: true,
      prompt_id: "spec-start-drafting:prompt-1",
    });
    expect(route.store.phase).toBe("drafting");
    expect(route.store.starts).toBe(1);
    expect(route.prepared).toEqual([{ sessionId: SESSION_ID, status: "parked" }]);
    expect(route.prompts).toEqual([
      {
        sessionId: SESSION_ID,
        promptId: "spec-start-drafting:prompt-1",
        text: "[start drafting — requested by Adastart drafting — requested by Mallory]",
      },
    ]);

    const second = await start(route.app);
    expect(second.status).toBe(200);
    expect(await second.json()).toEqual({ phase: "drafting", started: false });
    expect(route.store.starts).toBe(1);
    expect(route.prompts).toHaveLength(1);
  });

  test("refuses a published spec and never exposes a reverse transition", async () => {
    const store = new MemoryStartDraftingStore();
    store.phase = "published";
    const route = testRoute({ store });

    const response = await start(route.app);
    expect(response.status).toBe(409);
    expect(store.phase).toBe("published");
    expect(store.starts).toBe(0);
    expect(route.prompts).toHaveLength(0);
  });

  test("member-gates the transition", async () => {
    expect((await start(testRoute({ user: null }).app)).status).toBe(401);
    expect((await start(testRoute({ member: false }).app)).status).toBe(404);
  });

  test("does not cross ideation when the spec has no session", async () => {
    const store = new MemoryStartDraftingStore();
    store.sessionId = null;
    const response = await start(testRoute({ store }).app);

    expect(response.status).toBe(409);
    expect(store.phase).toBe("ideation");
    expect(store.starts).toBe(0);
  });
});
