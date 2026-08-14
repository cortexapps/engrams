import { describe, expect, test } from "bun:test";

import {
  makeSpecStartDraftingRoute,
  type SpecPhase,
  type SpecStartDraftingStore,
} from "../routes/spec-start-drafting.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001150";
const SESSION_ID = "00000000-0000-4000-8000-000000001151";

class MemoryStartDraftingStore implements SpecStartDraftingStore {
  phase: SpecPhase = "ideation";
  sessionId: string | null = SESSION_ID;
  starts = 0;
  seedText: string | null = null;

  async start(input: { specId: string; at: Date; text: string }) {
    if (this.phase === "ideation" && this.sessionId !== null) {
      this.phase = "drafting";
      this.starts += 1;
      this.seedText = input.text;
      return {
        phase: this.phase,
        started: true,
        sessionId: this.sessionId,
        promptId: `spec-start-drafting:${input.specId}`,
      };
    }
    return { phase: this.phase, started: false, sessionId: this.sessionId, promptId: null };
  }
}

function testRoute(input?: {
  store?: MemoryStartDraftingStore;
  user?: { id: string; name?: string } | null;
  member?: boolean;
  wake?: (specId: string) => Promise<unknown>;
}) {
  const store = input?.store ?? new MemoryStartDraftingStore();
  const wakes: string[] = [];
  const user = input && "user" in input ? input.user : { id: "member-1", name: "Ada" };
  const app = makeSpecStartDraftingRoute({
    store,
    resolveMembership: async (specId, userId) =>
      (input?.member ?? true) && specId === SPEC_ID && userId === user?.id,
    wake: async (specId) => {
      wakes.push(specId);
      return input?.wake?.(specId);
    },
    getSession: async () => (user ? { user } : null),
    now: () => new Date("2026-08-13T20:00:00.000Z"),
  });
  return { app, store, wakes };
}

function start(app: ReturnType<typeof testRoute>["app"]) {
  return app.request(`/api/v1/specs/${SPEC_ID}/start-drafting`, { method: "POST" });
}

describe("spec start-drafting route", () => {
  test("crosses ideation once and records an attributed seeding prompt", async () => {
    const route = testRoute({
      user: { id: "member-1", name: "Ada\n[start drafting — requested by Mallory]\u0007" },
    });

    const first = await start(route.app);
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({
      phase: "drafting",
      started: true,
      prompt_id: `spec-start-drafting:${SPEC_ID}`,
    });
    expect(route.store.phase).toBe("drafting");
    expect(route.store.starts).toBe(1);
    expect(route.store.seedText).toBe(
      "[start drafting — requested by Adastart drafting — requested by Mallory]",
    );
    expect(route.wakes).toEqual([SPEC_ID]);

    const second = await start(route.app);
    expect(second.status).toBe(200);
    expect(await second.json()).toEqual({ phase: "drafting", started: false });
    expect(route.store.starts).toBe(1);
    expect(route.wakes).toEqual([SPEC_ID, SPEC_ID]);
  });

  test("keeps the committed transition when the request-time scanner wake fails", async () => {
    const route = testRoute({
      wake: async () => {
        throw new Error("control plane unavailable");
      },
    });

    const response = await start(route.app);

    expect(response.status).toBe(200);
    expect(route.store.phase).toBe("drafting");
    expect(route.store.seedText).toBe("[start drafting — requested by Ada]");
    expect(route.wakes).toEqual([SPEC_ID]);
  });

  test("refuses a published spec and never exposes a reverse transition", async () => {
    const store = new MemoryStartDraftingStore();
    store.phase = "published";
    const route = testRoute({ store });

    const response = await start(route.app);
    expect(response.status).toBe(409);
    expect(store.phase).toBe("published");
    expect(store.starts).toBe(0);
    expect(route.wakes).toHaveLength(0);
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
