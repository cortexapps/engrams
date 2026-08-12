import { describe, expect, test } from "bun:test";

import {
  proposalDraftId,
  SpecTicketError,
  SpecTicketTreeService,
  type PinnedSpec,
  type PinnedSpecReader,
  type SpecTicketDraftRecord,
  type SpecTicketStore,
  type SpecTicketTreeView,
  type TicketProposalInput,
} from "../ticket-tree.ts";

const SPEC_ID = "0e2e3f2a-2f19-4a0b-9a3e-2c8c0b8a1f01";

const PINNED_SECTIONS = [
  { id: "sec-data", title: "Data model" },
  { id: "sec-api", title: "API" },
  { id: "sec-design", title: "Proposed design" },
  { id: "sec-failure", title: "Failure modes" },
];

/**
 * The pinned spec, and nothing else. A test that wants to prove the service
 * never reads the live head gives this reader the pinned sections only — the
 * live-Postgres suite proves the same property against a real moving head.
 */
class MemoryPinnedSpec implements PinnedSpecReader {
  reads = 0;

  constructor(private readonly pinned: PinnedSpec | null) {}

  async read(specId: string): Promise<PinnedSpec | null> {
    this.reads += 1;
    return this.pinned && this.pinned.specId === specId ? this.pinned : null;
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

function pinnedSpec(overrides: Partial<PinnedSpec> = {}): PinnedSpec {
  return {
    specId: SPEC_ID,
    checkpointId: "3d0f9e0e-9c26-4a71-8f2f-9a19b1c2d3e4",
    docSeq: 18n,
    publishedAt: new Date("2026-08-12T15:04:00.000Z"),
    sections: PINNED_SECTIONS,
    openQuestions: [],
    ...overrides,
  };
}

function setup(pinned: PinnedSpec | null = pinnedSpec()) {
  const store = new MemoryTicketStore();
  const reader = new MemoryPinnedSpec(pinned);
  let minted = 0;
  const service = new SpecTicketTreeService({
    store,
    pinned: reader,
    newId: () => `minted-${(minted += 1)}`,
  });
  return { service, store, reader };
}

/** The mock 2l proposal, as the agent sends it. */
const PROPOSAL: TicketProposalInput[] = [
  {
    client_id: "quota-columns",
    title: "Add org quota columns + backfill",
    description: "Add the columns and backfill them behind the flag.",
    section_id: "sec-data",
  },
  {
    client_id: "meter-rollup",
    title: "Hourly meter rollup job",
    description: "Roll the meter events up hourly.",
    section_id: "sec-data",
  },
  {
    client_id: "meter-events",
    parent_client_id: "meter-rollup",
    title: "Emit sandbox.created meter events",
    description: "Emit one event per sandbox.",
    section_id: "sec-api",
    depends_on: ["quota-columns"],
  },
  {
    client_id: "gateway-limiter",
    title: "Enforce org quota in the gateway limiter",
    description: "Enforce the cap in the limiter.",
    section_id: "sec-design",
  },
  {
    client_id: "payload-429",
    title: "Quota-aware 429 payload",
    description: "Return the remaining quota on a 429.",
    section_id: "sec-api",
  },
];

function ids(view: SpecTicketTreeView): string[] {
  return view.tickets.map((ticket) => ticket.title);
}

describe("the ticket proposal", () => {
  test("reads the pinned spec, and refuses a spec with no pin", async () => {
    const { service, reader } = setup(null);
    await expect(
      service.propose({ specId: SPEC_ID, idempotencyKey: "p1", tickets: PROPOSAL }),
    ).rejects.toThrow("not published");
    expect(reader.reads).toBe(1);
  });

  test("builds the tree the agent described", async () => {
    const { service } = setup();
    const { view, applied } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    expect(applied).toBe(true);
    expect(ids(view)).toEqual([
      "Add org quota columns + backfill",
      "Hourly meter rollup job",
      "Emit sandbox.created meter events",
      "Enforce org quota in the gateway limiter",
      "Quota-aware 429 payload",
    ]);
    const child = view.tickets.find((ticket) => ticket.title.startsWith("Emit"));
    const parent = view.tickets.find((ticket) => ticket.title.startsWith("Hourly"));
    expect(child?.parentId).toBe(parent?.id ?? "");
    expect(child?.depth).toBe(1);
    expect(child?.dependsOn).toEqual([
      proposalDraftId(SPEC_ID, "p1", "quota-columns"),
    ]);
  });

  test("every draft carries a resolvable backlink", async () => {
    const { service } = setup();
    const { view } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    const sectionIds = new Set(view.sections.map((section) => section.id));
    expect(view.tickets).toHaveLength(PROPOSAL.length);
    for (const ticket of view.tickets) {
      // The backlink names a section of the pinned spec …
      expect(sectionIds.has(ticket.backlink.sectionId)).toBe(true);
      expect(ticket.backlink.sectionTitle).toBe(
        PINNED_SECTIONS.find((section) => section.id === ticket.backlink.sectionId)?.title ?? "",
      );
      // … and the description opens with the link to it.
      expect(ticket.description.split("\n")[0]).toBe(
        `[§${ticket.backlink.sectionTitle}](${ticket.backlink.href})`,
      );
      expect(ticket.body).not.toContain("§");
    }
  });

  test("a proposal that cites a section the pinned spec lacks is refused whole", async () => {
    const { service, store } = setup();
    await expect(
      service.propose({
        specId: SPEC_ID,
        idempotencyKey: "p1",
        tickets: [
          ...PROPOSAL,
          {
            client_id: "ghost",
            title: "Ghost",
            description: "From a section that is not in the pin.",
            section_id: "sec-rollout",
          },
        ],
      }),
    ).rejects.toThrow("no section sec-rollout");
    expect(store.rows).toEqual([]);
  });

  test("an empty proposal is refused", async () => {
    const { service } = setup();
    await expect(
      service.propose({ specId: SPEC_ID, idempotencyKey: "p1", tickets: [] }),
    ).rejects.toThrow(SpecTicketError);
  });

  test("a replayed proposal lands one tree and keeps the edits since", async () => {
    const { service } = setup();
    const first = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    const target = first.view.tickets[0]?.id ?? "";
    await service.updateTicket({ specId: SPEC_ID, id: target, title: "Renamed by hand" });

    const replay = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    expect(replay.applied).toBe(false);
    expect(replay.view.tickets).toHaveLength(PROPOSAL.length);
    expect(replay.view.tickets[0]?.title).toBe("Renamed by hand");
  });

  test("a new proposal replaces the tree", async () => {
    const { service } = setup();
    await service.propose({ specId: SPEC_ID, idempotencyKey: "p1", tickets: PROPOSAL });
    const second = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p2",
      tickets: [PROPOSAL[4] as TicketProposalInput],
    });
    expect(second.applied).toBe(true);
    expect(ids(second.view)).toEqual(["Quota-aware 429 payload"]);
  });
});

describe("open questions on the tree (R29)", () => {
  const questions = [
    { id: "q6", sectionId: "sec-api", text: "Does the 429 carry the reset time?" },
    { id: "q7", sectionId: "sec-failure", text: "How long is the shadow count?" },
  ];

  test("a question rides on every ticket covering its section", async () => {
    const { service } = setup(pinnedSpec({ openQuestions: questions }));
    const { view } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    const carried = new Map(
      view.tickets.map((ticket) => [ticket.title, ticket.openQuestions.map((q) => q.id)]),
    );
    expect(carried.get("Emit sandbox.created meter events")).toEqual(["q6"]);
    expect(carried.get("Quota-aware 429 payload")).toEqual(["q6"]);
    expect(carried.get("Add org quota columns + backfill")).toEqual([]);
    expect(carried.get("Enforce org quota in the gateway limiter")).toEqual([]);
  });

  test("a question no ticket covers is reported, never dropped", async () => {
    const { service } = setup(pinnedSpec({ openQuestions: questions }));
    const { view } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    // No proposed ticket cites Failure modes, so q7 has nowhere to ride.
    expect(view.unattachedQuestions.map((question) => question.id)).toEqual(["q7"]);
  });

  test("a merge moves the question with the surviving backlink", async () => {
    const { service } = setup(pinnedSpec({ openQuestions: questions }));
    const { view } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    const events = view.tickets.find((ticket) => ticket.title.startsWith("Emit"));
    const payload = view.tickets.find((ticket) => ticket.title.startsWith("Quota-aware"));
    const merged = await service.mergeTickets({
      specId: SPEC_ID,
      targetId: payload?.id ?? "",
      sourceIds: [events?.id ?? ""],
    });
    const survivor = merged.tickets.find((ticket) => ticket.id === payload?.id);
    expect(merged.tickets).toHaveLength(PROPOSAL.length - 1);
    expect(survivor?.backlink.sectionTitle).toBe("API");
    expect(survivor?.openQuestions.map((question) => question.id)).toEqual(["q6"]);
  });

  test("re-pointing a backlink re-attaches the questions", async () => {
    const { service } = setup(pinnedSpec({ openQuestions: questions }));
    const { view } = await service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    const limiter = view.tickets.find((ticket) => ticket.title.startsWith("Enforce"));
    const next = await service.updateTicket({
      specId: SPEC_ID,
      id: limiter?.id ?? "",
      sectionId: "sec-failure",
    });
    const moved = next.tickets.find((ticket) => ticket.id === limiter?.id);
    expect(moved?.openQuestions.map((question) => question.id)).toEqual(["q7"]);
    expect(moved?.backlink.sectionTitle).toBe("Failure modes");
    expect(next.unattachedQuestions).toEqual([]);
  });
});

describe("direct manipulation through the service", () => {
  async function proposed() {
    const harness = setup();
    const { view } = await harness.service.propose({
      specId: SPEC_ID,
      idempotencyKey: "p1",
      tickets: PROPOSAL,
    });
    return { ...harness, view };
  }

  test("an added ticket gets its backlink written for it", async () => {
    const { service } = await proposed();
    const view = await service.addTicket({
      specId: SPEC_ID,
      parentId: null,
      index: 0,
      title: "Shadow-count 7 days before enforcing",
      description: "Count without enforcing first.",
      sectionId: "sec-failure",
    });
    const added = view.tickets[0];
    expect(added?.title).toBe("Shadow-count 7 days before enforcing");
    expect(added?.backlink.sectionTitle).toBe("Failure modes");
    expect(added?.description.startsWith("[§Failure modes](")).toBe(true);
    expect(added?.syncState).toBe("draft");
  });

  test("an added ticket cannot cite a section outside the pinned spec", async () => {
    const { service } = await proposed();
    await expect(
      service.addTicket({
        specId: SPEC_ID,
        parentId: null,
        title: "Ghost",
        description: "body",
        sectionId: "sec-rollout",
      }),
    ).rejects.toThrow("no section sec-rollout");
  });

  test("editing a description keeps exactly one backlink line", async () => {
    const { service, view } = await proposed();
    const target = view.tickets[0]?.id ?? "";
    const next = await service.updateTicket({
      specId: SPEC_ID,
      id: target,
      body: "Add the columns, then backfill in batches.",
    });
    const edited = next.tickets.find((ticket) => ticket.id === target);
    expect(edited?.body).toBe("Add the columns, then backfill in batches.");
    expect(edited?.description.split("\n\n")).toHaveLength(2);
  });

  test("a drag to nest re-parents and closes the gap it left", async () => {
    const { service, view } = await proposed();
    const limiter = view.tickets.find((ticket) => ticket.title.startsWith("Enforce"));
    const columns = view.tickets.find((ticket) => ticket.title.startsWith("Add org"));
    const next = await service.moveTicket({
      specId: SPEC_ID,
      id: limiter?.id ?? "",
      parentId: columns?.id ?? null,
    });
    const moved = next.tickets.find((ticket) => ticket.id === limiter?.id);
    expect(moved?.parentId).toBe(columns?.id ?? "");
    expect(moved?.depth).toBe(1);
    expect(next.tickets.filter((ticket) => ticket.parentId === null).map((t) => t.ordinal)).toEqual([
      0, 1, 2,
    ]);
  });

  test("a delete takes the subtree", async () => {
    const { service, view } = await proposed();
    const rollup = view.tickets.find((ticket) => ticket.title.startsWith("Hourly"));
    const next = await service.deleteTicket({ specId: SPEC_ID, id: rollup?.id ?? "" });
    expect(ids(next)).toEqual([
      "Add org quota columns + backfill",
      "Enforce org quota in the gateway limiter",
      "Quota-aware 429 payload",
    ]);
  });

  test("a split leaves two siblings, each with a backlink", async () => {
    const { service, view } = await proposed();
    const columns = view.tickets.find((ticket) => ticket.title.startsWith("Add org"));
    const next = await service.splitTicket({
      specId: SPEC_ID,
      id: columns?.id ?? "",
      parts: [
        { title: "Add org quota columns", body: "The migration." },
        { title: "Backfill the columns", body: "The backfill.", sectionId: "sec-failure" },
      ],
    });
    expect(ids(next).slice(0, 2)).toEqual(["Add org quota columns", "Backfill the columns"]);
    expect(next.tickets[0]?.backlink.sectionTitle).toBe("Data model");
    expect(next.tickets[1]?.backlink.sectionTitle).toBe("Failure modes");
    expect(next.tickets[1]?.description.startsWith("[§Failure modes](")).toBe(true);
  });

  test("a merged description keeps one backlink line, not one per part", async () => {
    const { service, view } = await proposed();
    const payload = view.tickets.find((ticket) => ticket.title.startsWith("Quota-aware"));
    const events = view.tickets.find((ticket) => ticket.title.startsWith("Emit"));
    const next = await service.mergeTickets({
      specId: SPEC_ID,
      targetId: payload?.id ?? "",
      sourceIds: [events?.id ?? ""],
    });
    const survivor = next.tickets.find((ticket) => ticket.id === payload?.id);
    expect(survivor?.description.match(/§/g)).toHaveLength(1);
    expect(survivor?.body).toBe("Return the remaining quota on a 429.\n\nEmit one event per sandbox.");
  });

  test("every operation needs a pinned spec", async () => {
    const { service } = setup(null);
    await expect(
      service.moveTicket({ specId: SPEC_ID, id: "any", parentId: null }),
    ).rejects.toThrow("not published");
    await expect(service.read(SPEC_ID)).rejects.toThrow("not published");
  });
});
