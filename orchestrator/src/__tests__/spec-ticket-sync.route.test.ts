/**
 * The Linear sync routes.
 *
 * The property worth guarding at this layer is R42: the ledger answers even
 * when Linear is not connected, so the UI can render the tree and point at the
 * connect page instead of showing an error where the tickets should be.
 */
import { describe, expect, test } from "bun:test";
import { Hono } from "hono";

import type { LinearIssue, LinearIssueClient } from "../integrations/linear-issues.ts";
import {
  makeSpecTicketSyncRoute,
  type SpecTicketSyncPayload,
} from "../routes/spec-ticket-sync.ts";
import { SpecTicketSyncService } from "../specs/ticket-sync-service.ts";
import type {
  ReserveInput,
  SpecTicketSyncConnector,
  SpecTicketSyncStore,
  SpecTicketSyncTarget,
  SyncOperationRow,
  SyncTicketRow,
} from "../specs/ticket-sync.ts";

const SPEC_ID = "0e2e3f2a-2f19-4a0b-9a3e-2c8c0b8a1f01";
const TICKET_ID = "11111111-1111-4111-8111-111111111111";
const OTHER_TICKET_ID = "22222222-2222-4222-8222-222222222222";
const MEMBER = "member-1";

/** The ledger, in memory. The Postgres one is proven by its own suite. */
class MemorySyncStore implements SpecTicketSyncStore {
  tickets: SyncTicketRow[] = [
    {
      id: TICKET_ID,
      parentId: null,
      ordinal: 0,
      title: "Add org quota columns",
      description: "Add the columns.",
      dependsOn: [],
      syncState: "draft",
      linearId: null,
      syncError: null,
    },
    {
      id: OTHER_TICKET_ID,
      parentId: null,
      ordinal: 1,
      title: "Hourly meter rollup job",
      description: "Roll it up.",
      dependsOn: [],
      syncState: "failed",
      linearId: null,
      syncError: "Linear returned 401 for create issue",
    },
  ];
  config: SpecTicketSyncTarget | null = null;
  operations: SyncOperationRow[] = [];

  async listTickets(): Promise<SyncTicketRow[]> {
    return this.tickets;
  }
  async readTicket(_specId: string, ticketId: string): Promise<SyncTicketRow | null> {
    return this.tickets.find((ticket) => ticket.id === ticketId) ?? null;
  }
  async writeTicketState(
    _specId: string,
    ticketId: string,
    patch: { syncState: SyncTicketRow["syncState"]; linearId?: string | null; syncError?: string | null },
  ): Promise<void> {
    this.tickets = this.tickets.map((ticket) =>
      ticket.id === ticketId
        ? {
            ...ticket,
            syncState: patch.syncState,
            linearId: patch.linearId === undefined ? ticket.linearId : patch.linearId,
            syncError: patch.syncError === undefined ? ticket.syncError : patch.syncError,
          }
        : ticket,
    );
  }
  async readConfig(): Promise<SpecTicketSyncTarget | null> {
    return this.config;
  }
  async writeConfig(_specId: string, target: SpecTicketSyncTarget): Promise<void> {
    this.config = target;
  }
  async reserve(input: ReserveInput): Promise<SyncOperationRow> {
    const row: SyncOperationRow = {
      callerSpecId: input.specId,
      operation: input.operation,
      idempotencyKey: input.idempotencyKey,
      requestHash: input.requestHash,
      status: "reserved",
      reservedTicketId: input.reservedTicketId,
      reservedExternalId: input.reservedExternalId,
      attempts: 0,
      result: null,
      error: null,
    };
    this.operations.push(row);
    return row;
  }
  async rehash(): Promise<void> {}
  async countAttempt(): Promise<number> {
    return 1;
  }
  async complete(): Promise<void> {}
  async fail(): Promise<void> {}
  async listOperations(): Promise<SyncOperationRow[]> {
    return this.operations;
  }
}

function connector(connected: boolean): SpecTicketSyncConnector {
  return {
    async read() {
      return {
        connected,
        reason: connected ? null : "Linear is not connected yet. Connect it in Settings.",
        defaults: {
          teamId: connected ? "team-platform" : null,
          teamName: connected ? "Platform" : null,
          projectId: null,
          projectName: null,
          labelIds: [],
          labelNames: [],
        },
      };
    },
  };
}

/** The route never calls Linear, except for the picker. */
const linear: LinearIssueClient = {
  async findIssue() {
    return null;
  },
  async createIssue(): Promise<LinearIssue> {
    throw new Error("the route never creates an issue");
  },
  async createBlockingRelation() {},
  async readWorkspace() {
    return {
      teams: [{ id: "team-platform", name: "Platform" }],
      projects: [],
      labels: [],
    };
  },
};

function testApp(options: { connected?: boolean; member?: boolean } = {}) {
  const app = new Hono();
  const store = new MemorySyncStore();
  const started: Array<{ ticketIds: string[]; workflowId: string }> = [];
  const sync = new SpecTicketSyncService({
    store,
    linear,
    connector: connector(options.connected ?? true),
    start: async (input, workflowId) => {
      started.push({ ticketIds: input.ticketIds, workflowId });
    },
  });
  app.route(
    "/",
    makeSpecTicketSyncRoute({
      sync,
      resolveMembership: async (specId, userId) =>
        (options.member ?? true) && specId === SPEC_ID && userId === MEMBER,
      getSession: async () => ({ user: { id: MEMBER, name: "Grace" } }),
      readWorkspace: () => linear.readWorkspace(),
    }),
  );
  return { app, store, started };
}

function get(path: string): Request {
  return new Request(`http://localhost${path}`);
}

function post(path: string, body?: unknown): Request {
  return new Request(`http://localhost${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

async function ledger(response: Response): Promise<SpecTicketSyncPayload> {
  expect(response.status).toBe(200);
  return (await response.json()) as SpecTicketSyncPayload;
}

describe("the spec ticket sync routes", () => {
  test("the ledger reports the target, the counts and the failure", async () => {
    const { app } = testApp();

    const payload = await ledger(await app.fetch(get(`/api/v1/specs/${SPEC_ID}/tickets/sync`)));

    expect(payload.connector).toEqual({ provider: "linear", connected: true, reason: null });
    expect(payload.target.teamName).toBe("Platform");
    expect(payload.total).toBe(2);
    expect(payload.failed).toBe(1);
    expect(payload.rows[1]?.error).toContain("401");
  });

  test("a sync queues the rows and starts one batch", async () => {
    const { app, started, store } = testApp();

    const payload = await ledger(await app.fetch(post(`/api/v1/specs/${SPEC_ID}/tickets/sync`)));

    expect(started).toHaveLength(1);
    expect(started[0]?.ticketIds).toEqual([TICKET_ID, OTHER_TICKET_ID]);
    expect(started[0]?.workflowId).toStartWith(`spec-ticket-sync:${SPEC_ID}:`);
    expect(payload.inFlight).toBe(2);
    expect(store.tickets.every((ticket) => ticket.syncState === "queued")).toBe(true);
  });

  test("one row can be retried on its own", async () => {
    const { app, started, store } = testApp();

    await ledger(await app.fetch(post(`/api/v1/specs/${SPEC_ID}/tickets/${OTHER_TICKET_ID}/sync`)));

    expect(started[0]?.ticketIds).toEqual([OTHER_TICKET_ID]);
    expect(store.tickets[0]?.syncState).toBe("draft");
    expect(store.tickets[1]?.syncState).toBe("queued");
  });

  test("a per-spec target is recorded and never leaves the spec", async () => {
    const { app, store } = testApp();

    const payload = await ledger(
      await app.fetch(
        post(`/api/v1/specs/${SPEC_ID}/tickets/sync`, {
          target: { teamId: "team-billing", teamName: "Billing", labelIds: ["l1"], labelNames: ["spec-mode"] },
        }),
      ),
    );

    expect(payload.overridden).toBe(true);
    expect(payload.target.teamName).toBe("Billing");
    expect(store.config?.teamId).toBe("team-billing");
  });

  test("no connector answers the ledger and refuses the sync", async () => {
    const { app, started } = testApp({ connected: false });

    // R42: the read still works, so the tree renders with a connect action.
    const payload = await ledger(await app.fetch(get(`/api/v1/specs/${SPEC_ID}/tickets/sync`)));
    expect(payload.connector.connected).toBe(false);
    expect(payload.connector.reason).toContain("not connected");
    expect(payload.total).toBe(2);

    const refused = await app.fetch(post(`/api/v1/specs/${SPEC_ID}/tickets/sync`));
    expect(refused.status).toBe(409);
    expect(await refused.json()).toMatchObject({ reason: "no_connector" });
    expect(started).toHaveLength(0);
  });

  test("a non-member sees a 404, not a ledger", async () => {
    const { app } = testApp({ member: false });

    expect((await app.fetch(get(`/api/v1/specs/${SPEC_ID}/tickets/sync`))).status).toBe(404);
    expect((await app.fetch(post(`/api/v1/specs/${SPEC_ID}/tickets/sync`))).status).toBe(404);
  });

  test("the picker offers the workspace's teams", async () => {
    const { app } = testApp();

    const response = await app.fetch(get(`/api/v1/specs/${SPEC_ID}/tickets/sync/targets`));

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ teams: [{ id: "team-platform", name: "Platform" }] });
  });

  test("a malformed target is a 400", async () => {
    const { app } = testApp();

    const response = await app.fetch(
      post(`/api/v1/specs/${SPEC_ID}/tickets/sync`, { target: { teamId: 7 } }),
    );

    expect(response.status).toBe(400);
  });
});
