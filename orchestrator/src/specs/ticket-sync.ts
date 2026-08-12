/**
 * Linear sync for the ticket tree (ADR 0114 D6, R41-R45, N4).
 *
 * The tree is drafts; this module turns a draft into a Linear issue, once. Four
 * rules shape everything here.
 *
 * **A failed row never blocks the others (R41).** Every ticket is its own
 * durable step. A step that fails writes the reason on its own row and returns;
 * it never throws into the batch. That is why the ledger in the UI can show one
 * red row between five green ones.
 *
 * **Create-only (R43).** A synced ticket is never written again. Later edits
 * belong in Linear, and the row keeps the Linear identity so a person can get
 * there in one click.
 *
 * **A retry never double-creates (N4).** Linear lets the caller choose an
 * issue's id. The sync derives that id from the spec and the ticket, records it
 * in the ledger before the call, and — on any attempt after the first — asks
 * Linear whether the issue already exists before it creates. A pod that dies
 * between the create and its own bookkeeping therefore adopts the issue it
 * already made.
 *
 * **An override is per spec (R45).** Team, project and labels default from the
 * org connector. A person may override them for one spec, and that override
 * lands in `spec_ticket_sync_config` — never back on the org default.
 *
 * The batch is driven by a DBOS workflow (../workflows/spec-ticket-sync.ts).
 * The functions here are the plain steps it runs, so a test drives them
 * directly and a pod roll resumes them instead of restarting them.
 */

import { createHash } from "node:crypto";

import type { SpecTicketSyncState } from "@engrams/spec-document";
import type { SpecTicketSyncOperationStatus } from "../db/schema.ts";
import { LinearError, type LinearIssue, type LinearIssueClient } from "../integrations/linear-issues.ts";
import { uuidV5 } from "./ticket-tree.ts";

/** Where a spec's tickets land in Linear (R45). */
export interface SpecTicketSyncTarget {
  teamId: string | null;
  teamName: string | null;
  projectId: string | null;
  projectName: string | null;
  labelIds: string[];
  labelNames: string[];
}

export const EMPTY_SYNC_TARGET: SpecTicketSyncTarget = {
  teamId: null,
  teamName: null,
  projectId: null,
  projectName: null,
  labelIds: [],
  labelNames: [],
};

/** What a person may change for one spec at sync time. */
export interface SpecTicketSyncOverrides {
  teamId?: string | null | undefined;
  teamName?: string | null | undefined;
  projectId?: string | null | undefined;
  projectName?: string | null | undefined;
  labelIds?: string[] | undefined;
  labelNames?: string[] | undefined;
}

/** The org connector's state and its defaults. */
export interface SpecTicketSyncConnectorState {
  /** False when no Linear connector is configured, or it is disabled (R42). */
  connected: boolean;
  /** Why it is not usable, in words a person can act on. */
  reason: string | null;
  defaults: SpecTicketSyncTarget;
}

export interface SpecTicketSyncConnector {
  read(): Promise<SpecTicketSyncConnectorState>;
}

/** One draft row, in the columns the sync reads and writes. */
export interface SyncTicketRow {
  id: string;
  parentId: string | null;
  ordinal: number;
  title: string;
  description: string;
  dependsOn: string[];
  syncState: SpecTicketSyncState;
  linearId: string | null;
  syncError: string | null;
}

/** One ledger row (`spec_ticket_sync_operation`). */
export interface SyncOperationRow {
  callerSpecId: string;
  operation: string;
  idempotencyKey: string;
  requestHash: string;
  status: SpecTicketSyncOperationStatus;
  reservedTicketId: string | null;
  reservedExternalId: string | null;
  attempts: number;
  result: Record<string, unknown> | null;
  error: string | null;
}

export interface ReserveInput {
  specId: string;
  operation: string;
  idempotencyKey: string;
  requestHash: string;
  reservedTicketId: string | null;
  reservedExternalId: string;
}

export interface SpecTicketSyncStore {
  listTickets(specId: string): Promise<SyncTicketRow[]>;
  readTicket(specId: string, ticketId: string): Promise<SyncTicketRow | null>;
  /** Write only the sync columns. The tree's own shape is not this module's. */
  writeTicketState(
    specId: string,
    ticketId: string,
    patch: { syncState: SpecTicketSyncState; linearId?: string | null; syncError?: string | null },
  ): Promise<void>;
  readConfig(specId: string): Promise<SpecTicketSyncTarget | null>;
  writeConfig(specId: string, target: SpecTicketSyncTarget): Promise<void>;
  /** Insert the reservation if it is new, and return the row that stands. */
  reserve(input: ReserveInput): Promise<SyncOperationRow>;
  /** Bind an existing reservation to a new request, after reconciliation. */
  rehash(specId: string, operation: string, key: string, requestHash: string): Promise<void>;
  countAttempt(specId: string, operation: string, key: string): Promise<number>;
  complete(
    specId: string,
    operation: string,
    key: string,
    result: Record<string, unknown>,
  ): Promise<void>;
  fail(specId: string, operation: string, key: string, error: string): Promise<void>;
  listOperations(specId: string): Promise<SyncOperationRow[]>;
}

export type SpecTicketSyncErrorCode = "conflict" | "not_found" | "no_connector" | "no_target";

export class SpecTicketSyncError extends Error {
  constructor(
    readonly code: SpecTicketSyncErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "SpecTicketSyncError";
  }
}

export const CREATE_ISSUE_OPERATION = "linear_issue_create";
export const CREATE_RELATION_OPERATION = "linear_relation_create";

/** The namespace under which a reserved Linear id is derived. */
const LINEAR_ID_NAMESPACE = "6a1f0c4b-6d2e-5f8a-9c3b-2e7d4a5b6c11";

/** The Linear issue id reserved for one draft ticket. Stable for its life. */
export function reservedIssueId(specId: string, ticketId: string): string {
  return uuidV5(`issue:${specId}:${ticketId}`, LINEAR_ID_NAMESPACE);
}

/** The Linear relation id reserved for one dependency edge. */
export function reservedRelationId(
  specId: string,
  blockedTicketId: string,
  blockerTicketId: string,
): string {
  return uuidV5(`relation:${specId}:${blockedTicketId}:${blockerTicketId}`, LINEAR_ID_NAMESPACE);
}

/** The stable DBOS workflow id of one spec's sync batch. */
export function syncWorkflowId(specId: string, batchKey: string): string {
  return `spec-ticket-sync:${specId}:${batchKey}`;
}

// ---------------------------------------------------------------------------
// The steps
// ---------------------------------------------------------------------------

export interface SpecTicketSyncDeps {
  store: SpecTicketSyncStore;
  linear: LinearIssueClient;
  connector: SpecTicketSyncConnector;
  log?: { warn: (object: unknown, message: string) => void };
}

export interface SpecTicketSyncPlan {
  specId: string;
  /** Ticket ids in dependency order, so a blocker has an identity first. */
  order: string[];
  target: SpecTicketSyncTarget;
}

/**
 * Read the batch: which tickets to sync, in which order, against which target.
 *
 * The order is topological over `depends_on`, so a blocker is created before
 * the ticket that waits for it and the waiting ticket's description can name a
 * real Linear identity (R44). A cycle cannot stall the batch — the remaining
 * tickets keep their tree order.
 */
export async function planSpecTicketSync(
  input: { specId: string; ticketIds?: readonly string[] | undefined },
  deps: SpecTicketSyncDeps,
): Promise<SpecTicketSyncPlan> {
  const target = await resolveTarget(input.specId, deps);
  const tickets = await deps.store.listTickets(input.specId);
  const wanted =
    input.ticketIds === undefined ? null : new Set(input.ticketIds.map((id) => id));
  const selected = tickets.filter(
    (ticket) => (wanted === null || wanted.has(ticket.id)) && ticket.syncState !== "synced",
  );
  return { specId: input.specId, order: dependencyOrder(selected), target };
}

/** The target a sync uses: the org defaults, with the spec's override on top. */
export async function resolveTarget(
  specId: string,
  deps: SpecTicketSyncDeps,
): Promise<SpecTicketSyncTarget> {
  const state = await deps.connector.read();
  if (!state.connected) {
    throw new SpecTicketSyncError(
      "no_connector",
      state.reason ?? "Linear is not connected for this organization.",
    );
  }
  const override = await deps.store.readConfig(specId);
  const target = mergeTarget(state.defaults, override);
  if (!target.teamId) {
    throw new SpecTicketSyncError(
      "no_target",
      "No Linear team is configured. Choose a team before syncing.",
    );
  }
  return target;
}

/** The spec's override wins field by field, so a partial override is honest. */
export function mergeTarget(
  defaults: SpecTicketSyncTarget,
  override: SpecTicketSyncTarget | null,
): SpecTicketSyncTarget {
  if (!override) return { ...defaults, labelIds: [...defaults.labelIds], labelNames: [...defaults.labelNames] };
  return {
    teamId: override.teamId ?? defaults.teamId,
    teamName: override.teamId ? override.teamName : defaults.teamName,
    projectId: override.projectId ?? defaults.projectId,
    projectName: override.projectId ? override.projectName : defaults.projectName,
    labelIds: override.labelIds.length > 0 ? [...override.labelIds] : [...defaults.labelIds],
    labelNames: override.labelIds.length > 0 ? [...override.labelNames] : [...defaults.labelNames],
  };
}

/**
 * What one ticket's step did. `gone` is the ticket a person deleted while the
 * batch was running: there is no row left to write a state on, and that is not
 * a failure of the batch.
 */
export type SyncOneOutcome = SpecTicketSyncState | "gone";

export interface SyncOneResult {
  ticketId: string;
  state: SyncOneOutcome;
  issue: LinearIssue | null;
  /** Why it failed, in the words the ledger shows. */
  error: string | null;
  /** True when Linear already had the issue and this attempt adopted it. */
  adopted: boolean;
}

/**
 * Sync one ticket. This is the whole N4 assertion in one function.
 *
 * **It never throws.** That is R41, and it is a property of this function rather
 * than of its caller: the batch is a bare loop of durable steps, so a step that
 * throws ends the workflow in `ERROR`, and the sweep re-adopts only `PENDING`
 * work — every later ticket would sit at `queued` with no driver until an
 * operator noticed. So everything that can fail for one row is guarded here: the
 * reads, the reservation, and the Linear call alike. A row that cannot even
 * record its own failure is logged and reported failed, because carrying on is
 * still the better answer for the five rows behind it.
 */
export async function syncOneSpecTicket(
  input: { specId: string; ticketId: string; target: SpecTicketSyncTarget },
  deps: SpecTicketSyncDeps,
): Promise<SyncOneResult> {
  try {
    return await createSpecTicketIssue(input, deps);
  } catch (error) {
    return recordTicketFailure(input.specId, input.ticketId, error, deps);
  }
}

/**
 * Write one row's failure. This is the last line of R41, so it swallows its own
 * errors: a driver that cannot reach Postgres to record a failure must still
 * return, or the batch it is in dies with it. The row then stays `syncing`,
 * which the ledger shows as in flight and a retry repairs.
 */
async function recordTicketFailure(
  specId: string,
  ticketId: string,
  error: unknown,
  deps: SpecTicketSyncDeps,
): Promise<SyncOneResult> {
  const reason = failureReason(error);
  try {
    await deps.store.fail(specId, CREATE_ISSUE_OPERATION, ticketId, reason);
    await deps.store.writeTicketState(specId, ticketId, {
      syncState: "failed",
      syncError: reason,
    });
  } catch (cause) {
    deps.log?.warn(
      { specId, ticketId, reason, cause: failureReason(cause) },
      "a spec ticket sync failure could not be recorded",
    );
  }
  return { ticketId, state: "failed", issue: null, error: reason, adopted: false };
}

/**
 * Stamp the draft row with an issue the ledger already holds. **Never throws.**
 *
 * By the time this runs, the reservation is `complete`: Linear made the issue
 * and the ledger names it. A dropped connection here must therefore not be
 * reported as a Linear failure — that report is the one state a retry could
 * double-create from, a ledger row saying `failed` about an issue that exists.
 * The row is left as it was instead, and the next attempt reads the complete
 * reservation and stamps the identity it already holds.
 */
async function stampSynced(
  specId: string,
  ticketId: string,
  issue: LinearIssue,
  deps: SpecTicketSyncDeps,
): Promise<void> {
  try {
    await deps.store.writeTicketState(specId, ticketId, {
      syncState: "synced",
      linearId: issue.id,
      syncError: null,
    });
  } catch (error) {
    deps.log?.warn(
      { specId, ticketId, issue: issue.identifier, reason: failureReason(error) },
      "a synced spec ticket could not be stamped; the ledger holds the issue",
    );
  }
}

async function createSpecTicketIssue(
  input: { specId: string; ticketId: string; target: SpecTicketSyncTarget },
  deps: SpecTicketSyncDeps,
): Promise<SyncOneResult> {
  const ticket = await deps.store.readTicket(input.specId, input.ticketId);
  if (!ticket) {
    // Deleted between the click and the driver. There is nothing to sync and
    // nothing to mark, so the step reports it and the batch moves on.
    return {
      ticketId: input.ticketId,
      state: "gone",
      issue: null,
      error: "The ticket was deleted before it synced.",
      adopted: false,
    };
  }
  // Create-only (R43). A synced ticket is finished, whatever the batch says.
  if (ticket.syncState === "synced" && ticket.linearId) {
    return { ticketId: ticket.id, state: "synced", issue: null, error: null, adopted: false };
  }

  const tickets = await deps.store.listTickets(input.specId);
  const operations = await deps.store.listOperations(input.specId);
  const description = issueDescription(ticket, tickets, operations);
  const request = {
    teamId: input.target.teamId,
    projectId: input.target.projectId,
    labelIds: input.target.labelIds,
    title: ticket.title,
    description,
  };

  const reservedExternalId = reservedIssueId(input.specId, ticket.id);
  const operation = await reserveSyncOperation(deps.store, {
    specId: input.specId,
    operation: CREATE_ISSUE_OPERATION,
    idempotencyKey: ticket.id,
    requestHash: requestHash(request),
    reservedTicketId: ticket.id,
    reservedExternalId,
  });

  // The reservation already carries a result: an earlier attempt finished and
  // this driver is replaying it. Re-state the row and make no call.
  const finished = readIssueResult(operation);
  if (operation.status === "complete" && finished) {
    await stampSynced(input.specId, ticket.id, finished, deps);
    return { ticketId: ticket.id, state: "synced", issue: finished, error: null, adopted: true };
  }

  await deps.store.writeTicketState(input.specId, ticket.id, {
    syncState: "syncing",
    syncError: null,
  });

  // Any attempt after the first may have created the issue and died before it
  // could say so. Ask Linear before creating a second one (N4).
  const existing = operation.attempts > 0 ? await deps.linear.findIssue(reservedExternalId) : null;
  // The count commits before the call, so a driver that dies during the create
  // still probes on its next attempt.
  await deps.store.countAttempt(input.specId, CREATE_ISSUE_OPERATION, ticket.id);
  const issue =
    existing ??
    (await deps.linear.createIssue({
      id: reservedExternalId,
      // `resolveTarget` refuses a batch with no team, so this is set.
      teamId: input.target.teamId ?? "",
      title: ticket.title,
      description,
      ...(input.target.projectId === null ? {} : { projectId: input.target.projectId }),
      ...(input.target.labelIds.length === 0 ? {} : { labelIds: input.target.labelIds }),
    }));
  await deps.store.complete(input.specId, CREATE_ISSUE_OPERATION, ticket.id, {
    id: issue.id,
    identifier: issue.identifier,
    url: issue.url,
  });

  // The issue exists and the ledger says so. Everything after this line is
  // bookkeeping about a fact that is already true.
  await stampSynced(input.specId, ticket.id, issue, deps);
  return {
    ticketId: ticket.id,
    state: "synced",
    issue,
    error: null,
    adopted: existing !== null,
  };
}

/**
 * Map `depends_on` onto Linear blocking relations (R44).
 *
 * This runs after the issues exist, and a failure here never moves a ticket off
 * `synced`: the issue is real, and the description already states the
 * dependency in prose. The relation is the better rendering of the same fact,
 * not a second source of truth.
 */
export async function linkSpecTicketDependencies(
  input: { specId: string },
  deps: SpecTicketSyncDeps,
): Promise<{ linked: number; failed: number }> {
  const tickets = await deps.store.listTickets(input.specId);
  const linearIdByTicket = new Map(
    tickets.filter((ticket) => ticket.linearId).map((ticket) => [ticket.id, ticket.linearId!]),
  );
  let linked = 0;
  let failed = 0;
  for (const ticket of tickets) {
    const blockedIssueId = linearIdByTicket.get(ticket.id);
    if (!blockedIssueId) continue;
    for (const blockerTicketId of ticket.dependsOn) {
      const blockerIssueId = linearIdByTicket.get(blockerTicketId);
      if (!blockerIssueId) continue;
      // Every edge is guarded whole — the reservation included. One edge that
      // cannot even be reserved must not cost the others their relation.
      try {
        if (await linkOneDependency({ ...input, ticket, blockerTicketId, blockerIssueId, blockedIssueId }, deps)) {
          linked += 1;
        }
      } catch (error) {
        const reason = failureReason(error);
        failed += 1;
        deps.log?.warn(
          { specId: input.specId, ticketId: ticket.id, blockerTicketId, reason },
          "spec ticket dependency was not linked in Linear",
        );
      }
    }
  }
  return { linked, failed };
}

/** One edge. True when this attempt created the relation. */
async function linkOneDependency(
  input: {
    specId: string;
    ticket: SyncTicketRow;
    blockerTicketId: string;
    blockerIssueId: string;
    blockedIssueId: string;
  },
  deps: SpecTicketSyncDeps,
): Promise<boolean> {
  const key = `${input.ticket.id}:${input.blockerTicketId}`;
  const reserved = reservedRelationId(input.specId, input.ticket.id, input.blockerTicketId);
  const operation = await reserveSyncOperation(deps.store, {
    specId: input.specId,
    operation: CREATE_RELATION_OPERATION,
    idempotencyKey: key,
    requestHash: requestHash({
      blockerIssueId: input.blockerIssueId,
      blockedIssueId: input.blockedIssueId,
    }),
    reservedTicketId: input.ticket.id,
    reservedExternalId: reserved,
  });
  if (operation.status === "complete") return false;
  try {
    await deps.store.countAttempt(input.specId, CREATE_RELATION_OPERATION, key);
    await deps.linear.createBlockingRelation({
      id: reserved,
      blockerIssueId: input.blockerIssueId,
      blockedIssueId: input.blockedIssueId,
    });
    await deps.store.complete(input.specId, CREATE_RELATION_OPERATION, key, { id: reserved });
    return true;
  } catch (error) {
    await deps.store.fail(input.specId, CREATE_RELATION_OPERATION, key, failureReason(error));
    throw error;
  }
}

/**
 * Take the reservation, and refuse a key reused with different arguments.
 *
 * The refusal is bounded to the case where it protects something: a **complete**
 * reservation. Its arguments are history — the issue exists, R43 forbids a
 * second write, and re-binding them would misreport what Linear holds.
 *
 * An unfinished reservation is re-bound instead, whether it is `failed` or
 * `reserved`. Both are attempts at the *same* ticket, because the key is the
 * ticket id, and the request legitimately drifts between attempts: the
 * description gains a "Blocked by ENG-412" line as soon as the blocker syncs
 * (R44). Refusing that drift turned an ordinary crash-recovery retry into a
 * fatal conflict. Re-binding costs nothing, because the reserved Linear id is
 * derived from the spec and the ticket and never moves — so the next attempt,
 * whose `attempts` count survived, probes Linear and adopts rather than
 * creating a second issue.
 */
export async function reserveSyncOperation(
  store: SpecTicketSyncStore,
  input: ReserveInput,
): Promise<SyncOperationRow> {
  const row = await store.reserve(input);
  if (row.requestHash === input.requestHash) return row;
  if (row.status === "complete") {
    throw new SpecTicketSyncError(
      "conflict",
      `idempotency key was already used with different arguments (${input.operation} ${input.idempotencyKey})`,
    );
  }
  await store.rehash(input.specId, input.operation, input.idempotencyKey, input.requestHash);
  return { ...row, requestHash: input.requestHash };
}

/**
 * The issue body: the draft's description, then the dependencies in prose.
 *
 * R44 asks for both arms. The relation is created after the issues exist, and a
 * relation can fail, so the description states the dependency too — a reader in
 * Linear learns what this waits for even when the relation never landed.
 */
export function issueDescription(
  ticket: SyncTicketRow,
  tickets: readonly SyncTicketRow[],
  operations: readonly SyncOperationRow[],
): string {
  if (ticket.dependsOn.length === 0) return ticket.description;
  const identifiers = new Map(
    operations
      .filter((operation) => operation.operation === CREATE_ISSUE_OPERATION)
      .flatMap((operation) => {
        const issue = readIssueResult(operation);
        return issue && operation.reservedTicketId
          ? ([[operation.reservedTicketId, issue.identifier]] as Array<[string, string]>)
          : [];
      }),
  );
  const byId = new Map(tickets.map((row) => [row.id, row]));
  const blockers = ticket.dependsOn
    .map((id) => {
      const blocker = byId.get(id);
      if (!blocker) return null;
      const identifier = identifiers.get(id);
      return identifier ? `${identifier} — ${blocker.title}` : blocker.title;
    })
    .filter((line): line is string => line !== null);
  if (blockers.length === 0) return ticket.description;
  return `${ticket.description}\n\n**Blocked by**\n${blockers.map((line) => `- ${line}`).join("\n")}`;
}

/** The issue an operation recorded, when it recorded one. */
export function readIssueResult(operation: SyncOperationRow): LinearIssue | null {
  const result = operation.result;
  if (!result) return null;
  const id = result["id"];
  const identifier = result["identifier"];
  const url = result["url"];
  if (typeof id !== "string" || typeof identifier !== "string" || typeof url !== "string") {
    return null;
  }
  return { id, identifier, url };
}

function failureReason(error: unknown): string {
  if (error instanceof LinearError) {
    return error.status === undefined ? error.message : `${error.message}`;
  }
  if (error instanceof SpecTicketSyncError) return error.message;
  return error instanceof Error ? error.message : String(error);
}

/** A canonical hash of the request, so a reuse with different arguments shows. */
export function requestHash(value: unknown): string {
  return sha256Hex(JSON.stringify(canonical(value)));
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value !== null && typeof value === "object") {
    const source = value as Record<string, unknown>;
    return Object.fromEntries(
      Object.keys(source)
        .sort()
        .map((key) => [key, canonical(source[key])]),
    );
  }
  return value;
}

function sha256Hex(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

/**
 * Tickets in dependency order.
 *
 * A blocker comes before what it blocks, so the waiting ticket's description
 * can name a real identity. Anything left in a cycle keeps its tree order —
 * the batch runs whatever the shape of the graph.
 */
export function dependencyOrder(tickets: readonly SyncTicketRow[]): string[] {
  const selectable = new Set(tickets.map((ticket) => ticket.id));
  const remaining = [...tickets];
  const done = new Set<string>();
  const order: string[] = [];
  while (remaining.length > 0) {
    const index = remaining.findIndex((ticket) =>
      ticket.dependsOn.every((id) => !selectable.has(id) || done.has(id)),
    );
    // A cycle: take the next row in tree order and carry on.
    const next = remaining.splice(index === -1 ? 0 : index, 1)[0];
    if (!next) break;
    done.add(next.id);
    order.push(next.id);
  }
  return order;
}
