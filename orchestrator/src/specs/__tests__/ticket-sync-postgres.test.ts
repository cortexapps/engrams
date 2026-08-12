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
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Ticket sync test template', '[]', $2,
               '{"alternatives":"on","talkItThrough":"suggested","gapCheck":"on"}')`,
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

    // A failed reservation is the one that may be re-bound: that is the Retry
    // button on a row a person edited after it failed. The reserved Linear id
    // does not change, so the retry still adopts rather than duplicates.
    await store.fail(specId, CREATE_ISSUE_OPERATION, ticketId, "Linear returned 401");
    const rebound = await reserveSyncOperation(store, {
      ...reservation,
      requestHash: requestHash({ title: "b" }),
    });
    expect(rebound.reservedExternalId).toBe(reservedIssueId(specId, ticketId));
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
