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

export interface SyncOneResult {
  ticketId: string;
  state: SpecTicketSyncState;
  issue: LinearIssue | null;
  /** Why it failed, in the words the ledger shows. */
  error: string | null;
  /** True when Linear already had the issue and this attempt adopted it. */
  adopted: boolean;
}

/**
 * Sync one ticket. This is the whole N4 assertion in one function.
 *
 * It never throws for a Linear failure: the reason lands on the row and the
 * batch carries on (R41). It throws only when the caller misused the ledger —
 * an idempotency key reused with different arguments — because that is a bug
 * in the caller, not a fact about Linear.
 */
export async function syncOneSpecTicket(
  input: { specId: string; ticketId: string; target: SpecTicketSyncTarget },
  deps: SpecTicketSyncDeps,
): Promise<SyncOneResult> {
  const ticket = await deps.store.readTicket(input.specId, input.ticketId);
  if (!ticket) {
    throw new SpecTicketSyncError("not_found", `Unknown ticket: ${input.ticketId}`);
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
    await deps.store.writeTicketState(input.specId, ticket.id, {
      syncState: "synced",
      linearId: finished.id,
      syncError: null,
    });
    return { ticketId: ticket.id, state: "synced", issue: finished, error: null, adopted: true };
  }

  await deps.store.writeTicketState(input.specId, ticket.id, {
    syncState: "syncing",
    syncError: null,
  });

  try {
    // Any attempt after the first may have created the issue and died before
    // it could say so. Ask Linear before creating a second one (N4).
    const existing =
      operation.attempts > 0 ? await deps.linear.findIssue(reservedExternalId) : null;
    // The count commits before the call, so a driver that dies during the
    // create still probes on its next attempt.
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
    await deps.store.writeTicketState(input.specId, ticket.id, {
      syncState: "synced",
      linearId: issue.id,
      syncError: null,
    });
    return {
      ticketId: ticket.id,
      state: "synced",
      issue,
      error: null,
      adopted: existing !== null,
    };
  } catch (error) {
    const reason = failureReason(error);
    await deps.store.fail(input.specId, CREATE_ISSUE_OPERATION, ticket.id, reason);
    await deps.store.writeTicketState(input.specId, ticket.id, {
      syncState: "failed",
      syncError: reason,
    });
    return { ticketId: ticket.id, state: "failed", issue: null, error: reason, adopted: false };
  }
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
      const key = `${ticket.id}:${blockerTicketId}`;
      const reserved = reservedRelationId(input.specId, ticket.id, blockerTicketId);
      const operation = await reserveSyncOperation(deps.store, {
        specId: input.specId,
        operation: CREATE_RELATION_OPERATION,
        idempotencyKey: key,
        requestHash: requestHash({ blockerIssueId, blockedIssueId }),
        reservedTicketId: ticket.id,
        reservedExternalId: reserved,
      });
      if (operation.status === "complete") continue;
      try {
        await deps.store.countAttempt(input.specId, CREATE_RELATION_OPERATION, key);
        await deps.linear.createBlockingRelation({
          id: reserved,
          blockerIssueId,
          blockedIssueId,
        });
        await deps.store.complete(input.specId, CREATE_RELATION_OPERATION, key, {
          id: reserved,
        });
        linked += 1;
      } catch (error) {
        const reason = failureReason(error);
        await deps.store.fail(input.specId, CREATE_RELATION_OPERATION, key, reason);
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

/**
 * Take the reservation, and refuse a key reused with different arguments.
 *
 * The one exception is a reservation that already failed: the person edited the
 * ticket and pressed Retry, which is the whole point of a failed row keeping
 * its place. Even then the reserved Linear id does not change, so the retry
 * still adopts an issue an earlier attempt may have created.
 */
export async function reserveSyncOperation(
  store: SpecTicketSyncStore,
  input: ReserveInput,
): Promise<SyncOperationRow> {
  const row = await store.reserve(input);
  if (row.requestHash === input.requestHash) return row;
  if (row.status !== "failed") {
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
