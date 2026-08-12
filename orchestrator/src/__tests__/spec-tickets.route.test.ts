import { describe, expect, test } from "bun:test";
import { Hono } from "hono";

import { makeSpecTicketRoute, type SpecTicketTreePayload } from "../routes/spec-tickets.ts";
import {
  SpecTicketTreeService,
  type PinnedSpec,
  type PinnedSpecReader,
  type SpecTicketDraftRecord,
  type SpecTicketStore,
} from "../specs/ticket-tree.ts";

const SPEC_ID = "0e2e3f2a-2f19-4a0b-9a3e-2c8c0b8a1f01";
const MEMBER = "member-1";

const SECTIONS = [
  { id: "sec-data", title: "Data model" },
  { id: "sec-api", title: "API" },
];

class MemoryPinnedSpec implements PinnedSpecReader {
  constructor(private readonly published = true) {}

  async read(specId: string): Promise<PinnedSpec | null> {
    if (!this.published || specId !== SPEC_ID) return null;
    return {
      specId,
      checkpointId: "3d0f9e0e-9c26-4a71-8f2f-9a19b1c2d3e4",
      docSeq: 18n,
      publishedAt: new Date("2026-08-12T15:04:00.000Z"),
      sections: SECTIONS,
      openQuestions: [{ id: "q6", sectionId: "sec-api", text: "Does it carry the reset time?" }],
    };
  }
}

class MemoryTicketStore implements SpecTicketStore {
  rows: SpecTicketDraftRecord[] = [];

  async list(specId: string): Promise<SpecTicketDraftRecord[]> {
    return this.rows.filter((row) => row.specId === specId);
  }

  async mutate(
    specId: string,
    apply: (current: SpecTicketDraftRecord[]) => SpecTicketDraftRecord[],
  ): Promise<SpecTicketDraftRecord[]> {
    const next = apply(await this.list(specId));
    this.rows = [...this.rows.filter((row) => row.specId !== specId), ...next];
    return next;
  }
}

function testApp(options: { published?: boolean; member?: boolean } = {}) {
  const app = new Hono();
  const store = new MemoryTicketStore();
  let minted = 0;
  const ids = [
    "11111111-1111-4111-8111-111111111111",
    "22222222-2222-4222-8222-222222222222",
    "33333333-3333-4333-8333-333333333333",
  ];
  const tickets = new SpecTicketTreeService({
    store,
    pinned: new MemoryPinnedSpec(options.published ?? true),
    newId: () => ids[minted++] ?? `spare-${minted}`,
  });
  app.route(
    "/",
    makeSpecTicketRoute({
      tickets,
      resolveMembership: async (specId, userId) =>
        (options.member ?? true) && specId === SPEC_ID && userId === MEMBER,
      getSession: async () => ({ user: { id: MEMBER, name: "Grace" } }),
    }),
  );
  return { app, tickets, store };
}

async function tree(response: Response): Promise<SpecTicketTreePayload> {
  expect(response.status).toBe(200);
  return (await response.json()) as SpecTicketTreePayload;
}

function post(path: string, body: unknown): Request {
  return new Request(`http://localhost${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

function patch(path: string, body: unknown): Request {
  return new Request(`http://localhost${path}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

async function seeded(app: Hono, tickets: SpecTicketTreeService) {
  await tickets.propose({
    specId: SPEC_ID,
    idempotencyKey: "p1",
    tickets: [
      {
        client_id: "columns",
        title: "Add org quota columns",
        description: "Add the columns.",
        section_id: "sec-data",
      },
      {
        client_id: "payload",
        title: "Quota-aware 429 payload",
        description: "Return the remaining quota.",
        section_id: "sec-api",
      },
    ],
  });
  return tree(await app.request(`/api/v1/specs/${SPEC_ID}/tickets`));
}

describe("spec ticket routes", () => {
  test("the tree carries the pin, the backlinks and the open questions", async () => {
    const { app, tickets } = testApp();
    const view = await seeded(app, tickets);
    expect(view.checkpointId).toBe("3d0f9e0e-9c26-4a71-8f2f-9a19b1c2d3e4");
    expect(view.docSeq).toBe("18");
    expect(view.publishedAt).toBe("2026-08-12T15:04:00.000Z");
    expect(view.sections).toEqual(SECTIONS);
    expect(view.tickets.map((ticket) => ticket.backlink.sectionTitle)).toEqual([
      "Data model",
      "API",
    ]);
    expect(view.tickets[1]?.openQuestions.map((question) => question.id)).toEqual(["q6"]);
    expect(view.tickets[0]?.body).toBe("Add the columns.");
  });

  test("a draft spec has no tree yet", async () => {
    const { app } = testApp({ published: false });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/tickets`);
    expect(response.status).toBe(409);
    expect((await response.json()) as { reason: string }).toMatchObject({
      reason: "not_published",
    });
  });

  test("a non-member cannot see the tree", async () => {
    const { app } = testApp({ member: false });
    expect((await app.request(`/api/v1/specs/${SPEC_ID}/tickets`)).status).toBe(404);
  });

  test("adding a ticket writes its backlink", async () => {
    const { app, tickets } = testApp();
    await seeded(app, tickets);
    const view = await tree(
      await app.request(
        post(`/api/v1/specs/${SPEC_ID}/tickets`, {
          parentId: null,
          index: 0,
          title: "Shadow-count first",
          body: "Count without enforcing.",
          sectionId: "sec-api",
        }),
      ),
    );
    expect(view.tickets[0]?.title).toBe("Shadow-count first");
    expect(view.tickets[0]?.description.startsWith("[§API](")).toBe(true);
    expect(view.tickets.map((ticket) => ticket.ordinal)).toEqual([0, 1, 2]);
  });

  test("a move re-parents and returns the whole tree", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const [first, second] = seed.tickets;
    const view = await tree(
      await app.request(
        post(`/api/v1/specs/${SPEC_ID}/tickets/${second?.id}/move`, { parentId: first?.id }),
      ),
    );
    expect(view.tickets.map((ticket) => ticket.depth)).toEqual([0, 1]);
    expect(view.tickets[1]?.parentId).toBe(first?.id ?? "");
  });

  test("a drop onto one's own child is refused", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const [first, second] = seed.tickets;
    await app.request(
      post(`/api/v1/specs/${SPEC_ID}/tickets/${second?.id}/move`, { parentId: first?.id }),
    );
    const response = await app.request(
      post(`/api/v1/specs/${SPEC_ID}/tickets/${first?.id}/move`, { parentId: second?.id }),
    );
    expect(response.status).toBe(409);
    expect((await response.json()) as { reason: string }).toMatchObject({ reason: "cycle" });
  });

  test("a merge folds one ticket into another", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const [first, second] = seed.tickets;
    const view = await tree(
      await app.request(
        post(`/api/v1/specs/${SPEC_ID}/tickets/${second?.id}/merge`, { sourceIds: [first?.id] }),
      ),
    );
    expect(view.tickets).toHaveLength(1);
    expect(view.tickets[0]?.id).toBe(second?.id ?? "");
    expect(view.tickets[0]?.body).toBe("Return the remaining quota.\n\nAdd the columns.");
  });

  test("a split needs two parts", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const response = await app.request(
      post(`/api/v1/specs/${SPEC_ID}/tickets/${seed.tickets[0]?.id}/split`, {
        parts: [{ title: "Only one", body: "a" }],
      }),
    );
    expect(response.status).toBe(400);
  });

  test("a split leaves two siblings", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const view = await tree(
      await app.request(
        post(`/api/v1/specs/${SPEC_ID}/tickets/${seed.tickets[0]?.id}/split`, {
          parts: [
            { title: "The migration", body: "a" },
            { title: "The backfill", body: "b", sectionId: "sec-api" },
          ],
        }),
      ),
    );
    expect(view.tickets.map((ticket) => ticket.title)).toEqual([
      "The migration",
      "The backfill",
      "Quota-aware 429 payload",
    ]);
    expect(view.tickets[1]?.backlink.sectionTitle).toBe("API");
  });

  test("retitling and rewriting keep one backlink line", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const view = await tree(
      await app.request(
        patch(`/api/v1/specs/${SPEC_ID}/tickets/${seed.tickets[0]?.id}`, {
          title: "Add org quota columns + backfill",
          body: "Add the columns, then backfill in batches.",
        }),
      ),
    );
    expect(view.tickets[0]?.title).toBe("Add org quota columns + backfill");
    expect(view.tickets[0]?.description.match(/§/g)).toHaveLength(1);
  });

  test("a backlink outside the pinned spec is refused", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const response = await app.request(
      patch(`/api/v1/specs/${SPEC_ID}/tickets/${seed.tickets[0]?.id}`, {
        sectionId: "sec-rollout",
      }),
    );
    expect(response.status).toBe(409);
    expect((await response.json()) as { reason: string }).toMatchObject({
      reason: "unknown_section",
    });
  });

  test("a delete takes the subtree", async () => {
    const { app, tickets } = testApp();
    const seed = await seeded(app, tickets);
    const [first, second] = seed.tickets;
    await app.request(
      post(`/api/v1/specs/${SPEC_ID}/tickets/${second?.id}/move`, { parentId: first?.id }),
    );
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/tickets/${first?.id}`, {
      method: "DELETE",
    });
    expect((await tree(response)).tickets).toEqual([]);
  });

  test("an unknown ticket id is a 404", async () => {
    const { app, tickets } = testApp();
    await seeded(app, tickets);
    const response = await app.request(
      `/api/v1/specs/${SPEC_ID}/tickets/44444444-4444-4444-8444-444444444444`,
      { method: "DELETE" },
    );
    expect(response.status).toBe(404);
  });
});
