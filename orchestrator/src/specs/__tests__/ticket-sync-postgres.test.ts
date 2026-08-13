/**
 * Linear sync against live Postgres (ADR 0114 D6, R41-R45, N4).
 *
 * The assertion that carries this suite is N4: a batch that dies in the middle
 * and is retried creates **zero** duplicate issues. A fake store could not
 * prove that, because the ledger is the thing under test — so the ledger here
 * is the real table, and the only fake is Linear itself.
 *
 * The fake Linear holds the issues it was told to create, exactly as Linear
 * would. That is what lets a test lose a response the way a rolling pod does:
 * the issue exists upstream, and this side never heard about it.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { schema as documentSchema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import {
  LinearError,
  type CreateLinearIssueInput,
  type CreateLinearRelationInput,
  type LinearIssue,
  type LinearIssueClient,
  type LinearWorkspace,
} from "../../integrations/linear-issues.ts";
import {
  PostgresPinnedSpecReader,
  PostgresSpecTicketStore,
  SpecTicketTreeService,
} from "../ticket-tree.ts";
import {
  linkSpecTicketDependencies,
  planSpecTicketSync,
  reservedIssueId,
  reserveSyncOperation,
  requestHash,
  syncOneSpecTicket,
  CREATE_ISSUE_OPERATION,
  SpecTicketSyncError,
  type SpecTicketSyncConnector,
  type SpecTicketSyncDeps,
  type SpecTicketSyncStore,
} from "../ticket-sync.ts";
import { PostgresSpecTicketSyncStore } from "../ticket-sync-store.ts";
import { SpecTicketSyncService } from "../ticket-sync-service.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 4 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

const NOW = new Date("2026-08-12T15:04:00.000Z");

const TEMPLATE_SECTIONS = [
  {
    key: "data",
    title: "Data model",
    layerKey: "contract",
    guidance: "Give the data model.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
  {
    key: "api",
    title: "API",
    layerKey: "contract",
    guidance: "Give the API.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
];

function encodedDocument(): Buffer {
  const document = documentSchema.nodes.doc!.create(
    null,
    [
      { id: "sec-data", key: "data", title: "Data model" },
      { id: "sec-api", key: "api", title: "API" },
    ].map((section) =>
      documentSchema.nodes.section!.create({ id: section.id, templateSectionKey: section.key }, [
        documentSchema.nodes.sectionHeading!.create(null, documentSchema.text(section.title)),
        documentSchema.nodes.paragraph!.create(null, [documentSchema.text("Body.")]),
      ]),
    ),
  );
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(document, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  const encoded = Buffer.from(Y.encodeStateAsUpdate(ydoc));
  ydoc.destroy();
  return encoded;
}

const PINNED_STATE = encodedDocument();

const PROPOSAL = [
  {
    client_id: "columns",
    title: "Add org quota columns + backfill",
    description: "Add the columns and backfill them.",
    section_id: "sec-data",
  },
  {
    client_id: "rollup",
    title: "Hourly meter rollup job",
    description: "Roll the meter up every hour.",
    section_id: "sec-data",
  },
  {
    client_id: "limiter",
    title: "Enforce org quota in the gateway limiter",
    description: "Enforce the quota at the gateway.",
    section_id: "sec-api",
    depends_on: ["columns"],
  },
  {
    client_id: "payload",
    title: "Quota-aware 429 payload",
    description: "Return the remaining quota on a 429.",
    section_id: "sec-api",
  },
];

/**
 * Linear, as far as this suite is concerned: a map of issues by the id the
 * caller chose, plus the two ways it can go wrong — a refusal, and a response
 * this side never receives.
 */
class FakeLinear implements LinearIssueClient {
  readonly issues = new Map<string, LinearIssue>();
  readonly created: string[] = [];
  readonly found: string[] = [];
  readonly relations: Array<{ blocker: string; blocked: string }> = [];
  /** Titles Linear refuses, and with what. */
  readonly refuse = new Map<string, LinearError>();
  /** Titles whose response is lost once, after Linear has already created. */
  readonly loseResponse = new Set<string>();
  /** The descriptions Linear was sent, by title. */
  readonly descriptions = new Map<string, string>();

  async findIssue(id: string): Promise<LinearIssue | null> {
    this.found.push(id);
    return this.issues.get(id) ?? null;
  }

  async createIssue(input: CreateLinearIssueInput): Promise<LinearIssue> {
    const refusal = this.refuse.get(input.title);
    if (refusal) throw refusal;
    if (this.issues.has(input.id)) {
      throw new LinearError("invalid_request", `Entity with id ${input.id} already exists.`);
    }
    this.created.push(input.id);
    this.descriptions.set(input.title, input.description);
    const issue: LinearIssue = {
      id: input.id,
      identifier: `ENG-${410 + this.issues.size}`,
      url: `https://linear.app/acme/issue/ENG-${410 + this.issues.size}`,
    };
    this.issues.set(input.id, issue);
    if (this.loseResponse.has(input.title)) {
      // Linear made the issue; this side never learns the answer. That is
      // exactly the state a pod roll leaves behind.
      this.loseResponse.delete(input.title);
      throw new Error("connection reset by peer");
    }
    return issue;
  }

  async createBlockingRelation(input: CreateLinearRelationInput): Promise<void> {
    this.relations.push({ blocker: input.blockerIssueId, blocked: input.blockedIssueId });
  }

  async readWorkspace(): Promise<LinearWorkspace> {
    return { teams: [{ id: "team-platform", name: "Platform" }], projects: [], labels: [] };
  }
}

/**
 * The real store with one method replaced.
 *
 * The delegations are written out rather than spread, because the store is a
 * class and a spread of an instance copies none of its prototype methods.
 */
function delegating(
  store: SpecTicketSyncStore,
  overrides: Partial<SpecTicketSyncStore>,
): SpecTicketSyncStore {
  return {
    listTickets: (specId) => store.listTickets(specId),
    readTicket: (specId, ticketId) => store.readTicket(specId, ticketId),
    writeTicketState: (specId, ticketId, patch) => store.writeTicketState(specId, ticketId, patch),
    readConfig: (specId) => store.readConfig(specId),
    writeConfig: (specId, target) => store.writeConfig(specId, target),
    reserve: (input) => store.reserve(input),
    rehash: (specId, operation, key, hash) => store.rehash(specId, operation, key, hash),
    countAttempt: (specId, operation, key) => store.countAttempt(specId, operation, key),
    complete: (specId, operation, key, result) => store.complete(specId, operation, key, result),
    fail: (specId, operation, key, error) => store.fail(specId, operation, key, error),
    listOperations: (specId) => store.listOperations(specId),
    ...overrides,
  };
}

function connectedTo(teamId: string | null): SpecTicketSyncConnector {
  return {
    async read() {
      return {
        connected: teamId !== null,
        reason: teamId === null ? "Linear is not connected yet." : null,
        defaults: {
          teamId,
          teamName: teamId === null ? null : "Platform",
          projectId: null,
          projectName: null,
          labelIds: [],
          labelNames: [],
        },
      };
    },
  };
}

describe("linear sync with live Postgres", () => {
  const templateId = randomUUID();
  const owner = `spec-sync-owner-${randomUUID()}`;
  const sessionId = randomUUID();
  const specIds: string[] = [];

  /** A published spec with a pinned checkpoint, ready to ticketize. */
  async function publishedSpec(): Promise<string> {
    const specId = randomUUID();
    const checkpointId = randomUUID();
    specIds.push(specId);
    await pool!.query(
      `INSERT INTO spec (id, org_id, owner_user_id, session_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, $3, $4, 'Org sandbox quotas', 'draft')`,
      [specId, owner, sessionId, templateId],
    );
    await pool!.query(
      `INSERT INTO spec_checkpoint
         (id, spec_id, state, state_vector, rendered_markdown, doc_seq, label, reason, created_at)
       VALUES ($1, $2, $3, $4, '# Org sandbox quotas', 18, 'Published version', 'publish', $5)`,
      [checkpointId, specId, PINNED_STATE, Buffer.from([0]), NOW],
    );
    await pool!.query(
      `UPDATE spec SET lifecycle = 'published', published_checkpoint_id = $2,
              published_by = $3, published_at = $4, current_semantic_doc_seq = 18
        WHERE id = $1`,
      [specId, checkpointId, owner, NOW],
    );
    return specId;
  }

  function tree(): SpecTicketTreeService {
    return new SpecTicketTreeService({
      store: new PostgresSpecTicketStore(pool!),
      pinned: new PostgresPinnedSpecReader(pool!),
      newId: () => randomUUID(),
    });
  }

  function deps(linear: FakeLinear, teamId: string | null = "team-platform"): SpecTicketSyncDeps {
    return {
      store: new PostgresSpecTicketSyncStore(pool!),
      linear,
      connector: connectedTo(teamId),
    };
  }

  /**
   * One batch, exactly as `SpecTicketSyncWorkflow` runs it: plan once, then one
   * step per ticket. The workflow adds durability around these calls; the calls
   * themselves are what decides whether a retry double-creates.
   */
  async function runBatch(specId: string, syncDeps: SpecTicketSyncDeps): Promise<void> {
    const plan = await planSpecTicketSync({ specId }, syncDeps);
    for (const ticketId of plan.order) {
      await syncOneSpecTicket({ specId, ticketId, target: plan.target }, syncDeps);
    }
    await linkSpecTicketDependencies({ specId }, syncDeps);
  }

  async function propose(specId: string): Promise<Map<string, string>> {
    const { view } = await tree().propose({
      specId,
      idempotencyKey: `sync-${specId}`,
      tickets: PROPOSAL,
    });
    return new Map(view.tickets.map((ticket) => [ticket.title, ticket.id]));
  }

  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Owner', $1 || '@example.test', false, now(), now())`,
      [owner],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections)
       VALUES ($1, 'Ticket sync test template', '[]', $2)`,
      [templateId, JSON.stringify(TEMPLATE_SECTIONS)],
    );
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [specIds]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    await pool.query('DELETE FROM "user" WHERE id = $1', [owner]);
    await pool.end();
  });

  test.skipIf(!reachable)(
    "a retry after a crash mid-batch creates zero duplicate tickets",
    async () => {
      const specId = await publishedSpec();
      const ids = await propose(specId);
      const linear = new FakeLinear();
      // Linear creates this one, and the answer never comes back — the pod
      // rolled between the create and the ledger write.
      linear.loseResponse.add("Hourly meter rollup job");

      await runBatch(specId, deps(linear));

      // Three of four landed; the fourth knows only that something broke.
      const store = new PostgresSpecTicketSyncStore(pool!);
      const afterCrash = await store.listTickets(specId);
      expect(afterCrash.filter((row) => row.syncState === "synced")).toHaveLength(3);
      expect(afterCrash.filter((row) => row.syncState === "failed")).toHaveLength(1);
      expect(linear.created).toHaveLength(4);
      expect(linear.issues.size).toBe(4);

      // The retry. It must adopt the issue Linear already has.
      await runBatch(specId, deps(linear));

      const afterRetry = await store.listTickets(specId);
      expect(afterRetry.every((row) => row.syncState === "synced")).toBe(true);
      // Four tickets, four issues, four creates. Not five.
      expect(linear.created).toHaveLength(4);
      expect(linear.issues.size).toBe(4);
      expect(new Set(afterRetry.map((row) => row.linearId)).size).toBe(4);

      // The adopted row carries the id the first attempt reserved.
      const rollupId = ids.get("Hourly meter rollup job")!;
      const rollup = afterRetry.find((row) => row.id === rollupId);
      expect(rollup?.linearId).toBe(reservedIssueId(specId, rollupId));
      expect(linear.found).toContain(reservedIssueId(specId, rollupId));

      // One ledger row per ticket, all complete.
      const operations = (await store.listOperations(specId)).filter(
        (operation) => operation.operation === CREATE_ISSUE_OPERATION,
      );
      expect(operations).toHaveLength(4);
      expect(operations.every((operation) => operation.status === "complete")).toBe(true);
    },
  );

  test.skipIf(!reachable)("a failed row leaves its siblings synced", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const linear = new FakeLinear();
    linear.refuse.set(
      "Enforce org quota in the gateway limiter",
      new LinearError("unauthenticated", "Linear returned 401 for create issue", 401),
    );

    await runBatch(specId, deps(linear));

    const store = new PostgresSpecTicketSyncStore(pool!);
    const rows = await store.listTickets(specId);
    const failed = rows.find((row) => row.id === ids.get("Enforce org quota in the gateway limiter"));
    expect(failed?.syncState).toBe("failed");
    expect(failed?.syncError).toContain("401");
    expect(failed?.linearId).toBeNull();
    // R41: the other three are done, and they kept their place.
    expect(rows.filter((row) => row.syncState === "synced")).toHaveLength(3);
    expect(rows.map((row) => row.ordinal)).toEqual([0, 1, 2, 3]);

    // The ledger says the same thing, with the reason a person reads.
    const view = await new SpecTicketSyncService({
      ...deps(linear),
      start: async () => {},
    }).read(specId);
    expect(view.synced).toBe(3);
    expect(view.failed).toBe(1);
    expect(view.rows.find((row) => row.syncState === "failed")?.error).toContain("401");
    expect(view.rows.filter((row) => row.issue !== null)).toHaveLength(3);

    // And a retry after the credential is fixed syncs only that row.
    linear.refuse.clear();
    await runBatch(specId, deps(linear));
    expect((await store.listTickets(specId)).every((row) => row.syncState === "synced")).toBe(true);
    expect(linear.created).toHaveLength(4);
  });

  test.skipIf(!reachable)("reusing an idempotency key with different arguments fails", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const ticketId = ids.get("Quota-aware 429 payload")!;
    const store = new PostgresSpecTicketSyncStore(pool!);
    const reservation = {
      specId,
      operation: CREATE_ISSUE_OPERATION,
      idempotencyKey: ticketId,
      reservedTicketId: ticketId,
      reservedExternalId: reservedIssueId(specId, ticketId),
    };

    await reserveSyncOperation(store, { ...reservation, requestHash: requestHash({ title: "a" }) });
    await store.complete(specId, CREATE_ISSUE_OPERATION, ticketId, { id: "x" });

    await expect(
      reserveSyncOperation(store, { ...reservation, requestHash: requestHash({ title: "b" }) }),
    ).rejects.toThrow(/already used with different arguments/);

    // The refusal holds even after a later error, because a complete
    // reservation can no longer be pushed back to failed.
    await store.fail(specId, CREATE_ISSUE_OPERATION, ticketId, "connection terminated");
    await expect(
      reserveSyncOperation(store, { ...reservation, requestHash: requestHash({ title: "b" }) }),
    ).rejects.toThrow(/already used with different arguments/);

    // An UNFINISHED reservation is the one that may be re-bound: that is the
    // Retry button on a row a person edited after it failed. The reserved
    // Linear id does not change, so the retry adopts rather than duplicates.
    const otherId = ids.get("Hourly meter rollup job")!;
    const unfinished = {
      specId,
      operation: CREATE_ISSUE_OPERATION,
      idempotencyKey: otherId,
      reservedTicketId: otherId,
      reservedExternalId: reservedIssueId(specId, otherId),
    };
    await reserveSyncOperation(store, { ...unfinished, requestHash: requestHash({ title: "a" }) });
    await store.fail(specId, CREATE_ISSUE_OPERATION, otherId, "Linear returned 401");
    const rebound = await reserveSyncOperation(store, {
      ...unfinished,
      requestHash: requestHash({ title: "b" }),
    });
    expect(rebound.reservedExternalId).toBe(reservedIssueId(specId, otherId));
    expect(rebound.requestHash).toBe(requestHash({ title: "b" }));
  });

  test.skipIf(!reachable)("no connector still leaves the tree editable", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const linear = new FakeLinear();
    const service = new SpecTicketSyncService({
      ...deps(linear, null),
      start: async () => {},
    });

    // R42: the sync refuses, and says what to do about it.
    await expect(service.start({ specId })).rejects.toThrow(/not connected/);
    expect(linear.created).toHaveLength(0);

    // The ledger still answers, so the UI can show the connect call to action.
    const view = await service.read(specId);
    expect(view.connector.connected).toBe(false);
    expect(view.connector.reason).toContain("not connected");
    expect(view.total).toBe(4);
    expect(view.synced).toBe(0);

    // And every tree edit still works, which is the whole of R42.
    const renamed = await tree().updateTicket({
      specId,
      id: ids.get("Hourly meter rollup job")!,
      title: "Hourly meter rollup",
    });
    expect(renamed.tickets.map((ticket) => ticket.title)).toContain("Hourly meter rollup");
    const added = await tree().addTicket({
      specId,
      parentId: null,
      title: "Shadow-count 7 days before enforcing",
      description: "Count without enforcing.",
      sectionId: "sec-api",
    });
    expect(added.tickets).toHaveLength(5);
    const deleted = await tree().deleteTicket({
      specId,
      id: ids.get("Quota-aware 429 payload")!,
    });
    expect(deleted.tickets).toHaveLength(4);
  });

  test.skipIf(!reachable)("a synced ticket is never created twice", async () => {
    const specId = await publishedSpec();
    await propose(specId);
    const linear = new FakeLinear();

    await runBatch(specId, deps(linear));
    expect(linear.created).toHaveLength(4);

    // R43: create-only. A second batch has nothing to do, and asks Linear
    // nothing at all.
    const before = linear.found.length;
    await runBatch(specId, deps(linear));
    expect(linear.created).toHaveLength(4);
    expect(linear.found).toHaveLength(before);
  });

  test.skipIf(!reachable)("a dependency becomes a blocking relation and a line of prose", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const linear = new FakeLinear();

    await runBatch(specId, deps(linear));

    // R44, both arms: the relation, and the description that survives without it.
    const blocker = reservedIssueId(specId, ids.get("Add org quota columns + backfill")!);
    const blocked = reservedIssueId(specId, ids.get("Enforce org quota in the gateway limiter")!);
    expect(linear.relations).toEqual([{ blocker, blocked }]);
    const description = linear.descriptions.get("Enforce org quota in the gateway limiter") ?? "";
    expect(description).toContain("**Blocked by**");
    expect(description).toContain("Add org quota columns + backfill");

    // A replay adds no second relation.
    await linkSpecTicketDependencies({ specId }, deps(linear));
    expect(linear.relations).toHaveLength(1);
  });

  test.skipIf(!reachable)("a per-spec target never writes back to the org default", async () => {
    const specId = await publishedSpec();
    await propose(specId);
    const linear = new FakeLinear();
    const connector = connectedTo("team-platform");
    const service = new SpecTicketSyncService({
      ...deps(linear),
      connector,
      start: async () => {},
    });

    await service.start({
      specId,
      overrides: {
        teamId: "team-billing",
        teamName: "Billing",
        projectId: "project-quota",
        projectName: "Quota & billing",
        labelIds: ["label-spec-mode"],
        labelNames: ["spec-mode"],
      },
    });

    // R45: the spec's row carries the override …
    const view = await service.read(specId);
    expect(view.overridden).toBe(true);
    expect(view.target.teamId).toBe("team-billing");
    expect(view.target.teamName).toBe("Billing");
    expect(view.target.labelNames).toEqual(["spec-mode"]);

    // … and the org default is what it always was.
    expect((await connector.read()).defaults.teamId).toBe("team-platform");
    const other = await publishedSpec();
    expect((await service.read(other)).target.teamId).toBe("team-platform");
  });

  test.skipIf(!reachable)(
    "a row that throws before its Linear call still lets the batch finish",
    async () => {
      const specId = await publishedSpec();
      const ids = await propose(specId);
      const linear = new FakeLinear();
      const store = new PostgresSpecTicketSyncStore(pool!);
      const broken = ids.get("Enforce org quota in the gateway limiter")!;
      // A transient Postgres error on one row's first read — before the
      // reservation and before any Linear call. That region used to sit
      // outside the guard, so the throw ended the workflow and left every
      // later row at `queued` with no driver: the exact shape R41 forbids.
      const flaky = delegating(store, {
        readTicket: (spec, ticketId) =>
          ticketId === broken
            ? Promise.reject(new Error("connection terminated unexpectedly"))
            : store.readTicket(spec, ticketId),
      });

      const plan = await planSpecTicketSync({ specId }, deps(linear));
      const outcomes: string[] = [];
      for (const ticketId of plan.order) {
        // The assertion is that this never throws, whatever one row does.
        const result = await syncOneSpecTicket(
          { specId, ticketId, target: plan.target },
          { ...deps(linear), store: flaky },
        );
        outcomes.push(result.state);
      }

      expect(outcomes).toEqual(["synced", "synced", "failed", "synced"]);
      const rows = await store.listTickets(specId);
      // R41: the three siblings landed, and the row that threw carries the
      // reason instead of stranding the batch.
      expect(rows.filter((row) => row.syncState === "synced")).toHaveLength(3);
      const failed = rows.find((row) => row.id === broken);
      expect(failed?.syncState).toBe("failed");
      expect(failed?.syncError).toContain("connection terminated");
    },
  );

  test.skipIf(!reachable)(
    "a bookkeeping error after a real create never becomes a duplicate",
    async () => {
      const specId = await publishedSpec();
      const ids = await propose(specId);
      const linear = new FakeLinear();
      const store = new PostgresSpecTicketSyncStore(pool!);
      const stumbles = ids.get("Hourly meter rollup job")!;
      let dropped = false;
      // Linear creates the issue, the ledger records it, and the connection
      // drops on the very next write — the stamp of the draft row. One fault,
      // ordinary infra blip.
      const flaky = delegating(store, {
        writeTicketState: (spec, ticketId, patch) => {
          if (!dropped && ticketId === stumbles && patch.syncState === "synced") {
            dropped = true;
            return Promise.reject(new Error("connection terminated unexpectedly"));
          }
          return store.writeTicketState(spec, ticketId, patch);
        },
      });

      const plan = await planSpecTicketSync({ specId }, deps(linear));
      for (const ticketId of plan.order) {
        await syncOneSpecTicket(
          { specId, ticketId, target: plan.target },
          { ...deps(linear), store: flaky },
        );
      }

      expect(dropped).toBe(true);
      expect(linear.created).toHaveLength(4);
      // The ledger keeps the truth: the issue exists, so its row stays
      // complete. A row that said `failed` here is the one state a retry could
      // create a second issue from.
      const operation = (await store.listOperations(specId)).find(
        (row) => row.idempotencyKey === stumbles,
      );
      expect(operation?.status).toBe("complete");
      expect(operation?.result?.["identifier"]).toBeString();
      expect(operation?.error).toBeNull();

      // The retry reads that reservation and stamps the identity it already
      // holds. It asks Linear for nothing and creates nothing.
      const before = linear.found.length;
      await syncOneSpecTicket({ specId, ticketId: stumbles, target: plan.target }, deps(linear));

      expect(linear.created).toHaveLength(4);
      expect(linear.issues.size).toBe(4);
      expect(linear.found).toHaveLength(before);
      const row = (await store.listTickets(specId)).find((entry) => entry.id === stumbles);
      expect(row?.syncState).toBe("synced");
      expect(row?.linearId).toBe(reservedIssueId(specId, stumbles));
    },
  );

  test.skipIf(!reachable)("a complete reservation is never downgraded to failed", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const ticketId = ids.get("Add org quota columns + backfill")!;
    const store = new PostgresSpecTicketSyncStore(pool!);
    await reserveSyncOperation(store, {
      specId,
      operation: CREATE_ISSUE_OPERATION,
      idempotencyKey: ticketId,
      requestHash: requestHash({ title: "a" }),
      reservedTicketId: ticketId,
      reservedExternalId: reservedIssueId(specId, ticketId),
    });
    await store.complete(specId, CREATE_ISSUE_OPERATION, ticketId, {
      id: reservedIssueId(specId, ticketId),
      identifier: "ENG-412",
      url: "https://linear.app/acme/issue/ENG-412",
    });

    // The guard is the table's, not the caller's discipline.
    await store.fail(specId, CREATE_ISSUE_OPERATION, ticketId, "connection terminated");

    const operation = (await store.listOperations(specId)).find(
      (row) => row.idempotencyKey === ticketId,
    );
    expect(operation?.status).toBe("complete");
    expect(operation?.error).toBeNull();
    expect(operation?.result?.["identifier"]).toBe("ENG-412");
  });

  test.skipIf(!reachable)("a deleted ticket is reported, not thrown", async () => {
    const specId = await publishedSpec();
    const ids = await propose(specId);
    const linear = new FakeLinear();
    const removed = ids.get("Quota-aware 429 payload")!;
    const plan = await planSpecTicketSync({ specId }, deps(linear));
    await tree().deleteTicket({ specId, id: removed });

    const result = await syncOneSpecTicket(
      { specId, ticketId: removed, target: plan.target },
      deps(linear),
    );

    expect(result.state).toBe("gone");
    expect(linear.created).toHaveLength(0);
  });

  test.skipIf(!reachable)(
    "a reserved row whose request drifted is re-bound, not refused",
    async () => {
      const specId = await publishedSpec();
      const ids = await propose(specId);
      const linear = new FakeLinear();
      const store = new PostgresSpecTicketSyncStore(pool!);
      const blocked = ids.get("Enforce org quota in the gateway limiter")!;
      const plan = await planSpecTicketSync({ specId }, deps(linear));

      // The blocked ticket is attempted while its blocker has no identity yet,
      // and its driver dies mid-create: Linear holds the issue, the ledger row
      // stays unfinished.
      linear.loseResponse.add("Enforce org quota in the gateway limiter");
      await syncOneSpecTicket({ specId, ticketId: blocked, target: plan.target }, deps(linear));
      await pool!.query(
        `UPDATE spec_ticket_sync_operation SET status = 'reserved'
          WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3`,
        [specId, CREATE_ISSUE_OPERATION, blocked],
      );

      // Now the blocker syncs, so the blocked ticket's description gains its
      // "Blocked by ENG-…" line (R44) and its request hash legitimately moves.
      for (const ticketId of plan.order.filter((id) => id !== blocked)) {
        await syncOneSpecTicket({ specId, ticketId, target: plan.target }, deps(linear));
      }
      const retried = await syncOneSpecTicket(
        { specId, ticketId: blocked, target: plan.target },
        deps(linear),
      );

      // The drift is not a conflict: it adopts the issue the dead driver made,
      // and creates nothing new.
      expect(retried.state).toBe("synced");
      expect(retried.adopted).toBe(true);
      expect(linear.created).toHaveLength(4);
      expect(linear.issues.size).toBe(4);
      const reserved = (await store.listOperations(specId)).find(
        (operation) => operation.idempotencyKey === blocked,
      );
      expect(reserved?.status).toBe("complete");
    },
  );

  test.skipIf(!reachable)("an unknown ticket is refused, not queued", async () => {
    const specId = await publishedSpec();
    await propose(specId);
    const service = new SpecTicketSyncService({
      ...deps(new FakeLinear()),
      start: async () => {},
    });
    await expect(
      service.start({ specId, ticketIds: [randomUUID()] }),
    ).rejects.toBeInstanceOf(SpecTicketSyncError);
  });
});
