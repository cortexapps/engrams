/**
 * The Linear sync service: what the routes and the workflow both call.
 *
 * The batch itself is durable and lives in DBOS
 * (../workflows/spec-ticket-sync.ts). This service is the thin layer around it:
 * it resolves the target, records the per-spec override, moves the chosen rows
 * to `queued`, starts the workflow under a stable id, and reads back the ledger
 * the UI renders.
 *
 * Starting is separate from running for the reason ADR 0034 gives: a person who
 * asks for a sync must not depend on the request that asked for it. The request
 * records the intent and returns; a driver finishes the work, whatever happens
 * to the pod that took the click.
 */

import type { SpecTicketSyncState } from "@engrams/spec-document";
import type { LinearIssue } from "../integrations/linear-issues.ts";
import {
  mergeTarget,
  readIssueResult,
  requestHash,
  resolveTarget,
  syncWorkflowId,
  CREATE_ISSUE_OPERATION,
  SpecTicketSyncError,
  type SpecTicketSyncDeps,
  type SpecTicketSyncOverrides,
  type SpecTicketSyncTarget,
} from "./ticket-sync.ts";

/** One row of the ledger the UI renders (mock 2l). */
export interface SpecTicketSyncRowView {
  ticketId: string;
  title: string;
  syncState: SpecTicketSyncState;
  /** The Linear identity, once the ticket has one (R43). */
  issue: LinearIssue | null;
  error: string | null;
}

/** The whole sync rail: the target, the progress and the failures. */
export interface SpecTicketSyncView {
  specId: string;
  connector: { provider: string; connected: boolean; reason: string | null };
  /** The target a sync would use now: org defaults under the spec override. */
  target: SpecTicketSyncTarget;
  /** True when this spec has an override of its own (R45). */
  overridden: boolean;
  total: number;
  synced: number;
  failed: number;
  /** Rows a driver is working on, or has been asked to work on. */
  inFlight: number;
  rows: SpecTicketSyncRowView[];
}

export interface SpecTicketSyncStartInput {
  specId: string;
  /** Absent means every row that is not synced yet. */
  ticketIds?: readonly string[] | undefined;
  overrides?: SpecTicketSyncOverrides | undefined;
}

/** Starts the durable batch. Faked in tests; DBOS in production. */
export type SpecTicketSyncStarter = (
  input: { specId: string; ticketIds: string[] },
  workflowId: string,
) => Promise<void>;

export interface SpecTicketSyncServiceOptions extends SpecTicketSyncDeps {
  start: SpecTicketSyncStarter;
  /** The batch key of a sync, so a repeated click resumes one workflow. */
  batchKey?: () => string;
}

export class SpecTicketSyncService {
  constructor(private readonly options: SpecTicketSyncServiceOptions) {}

  /** The ledger, whether or not Linear is connected (R42). */
  async read(specId: string): Promise<SpecTicketSyncView> {
    const state = await this.options.connector.read();
    const override = await this.options.store.readConfig(specId);
    const tickets = await this.options.store.listTickets(specId);
    const operations = await this.options.store.listOperations(specId);
    const issues = new Map(
      operations
        .filter((operation) => operation.operation === CREATE_ISSUE_OPERATION)
        .flatMap((operation) => {
          const issue = readIssueResult(operation);
          return issue && operation.reservedTicketId
            ? ([[operation.reservedTicketId, issue]] as Array<[string, LinearIssue]>)
            : [];
        }),
    );
    const rows = tickets.map((ticket) => ({
      ticketId: ticket.id,
      title: ticket.title,
      syncState: ticket.syncState,
      issue: issues.get(ticket.id) ?? null,
      error: ticket.syncError,
    }));
    return {
      specId,
      connector: {
        provider: "linear",
        connected: state.connected,
        reason: state.reason,
      },
      target: mergeTarget(state.defaults, override),
      overridden: override !== null,
      total: rows.length,
      synced: rows.filter((row) => row.syncState === "synced").length,
      failed: rows.filter((row) => row.syncState === "failed").length,
      inFlight: rows.filter((row) => row.syncState === "queued" || row.syncState === "syncing")
        .length,
      rows,
    };
  }

  /**
   * Record the intent and hand the batch to the workflow.
   *
   * A row already synced is never queued again (R43). Everything else the
   * caller named moves to `queued`, which is what the tree renders while it
   * waits for a driver.
   */
  async start(input: SpecTicketSyncStartInput): Promise<SpecTicketSyncView> {
    if (input.overrides) {
      await this.writeOverride(input.specId, input.overrides);
    }
    // Refuses a batch with no connector or no team, so the person learns why
    // here rather than in a row that fails six times.
    await resolveTarget(input.specId, this.options);

    const tickets = await this.options.store.listTickets(input.specId);
    const wanted = input.ticketIds === undefined ? null : new Set(input.ticketIds);
    if (wanted) {
      const unknown = [...wanted].filter(
        (id) => !tickets.some((ticket) => ticket.id === id),
      );
      if (unknown.length > 0) {
        throw new SpecTicketSyncError("not_found", `Unknown ticket: ${unknown.join(", ")}`);
      }
    }
    const queued = tickets.filter(
      (ticket) => (wanted === null || wanted.has(ticket.id)) && ticket.syncState !== "synced",
    );
    for (const ticket of queued) {
      await this.options.store.writeTicketState(input.specId, ticket.id, {
        syncState: "queued",
        syncError: null,
      });
    }
    if (queued.length > 0) {
      const ids = queued.map((ticket) => ticket.id);
      await this.options.start(
        { specId: input.specId, ticketIds: ids },
        syncWorkflowId(input.specId, this.options.batchKey?.() ?? batchKeyOf(ids)),
      );
    }
    return this.read(input.specId);
  }

  /** The per-spec override (R45). It never touches the org default. */
  private async writeOverride(
    specId: string,
    overrides: SpecTicketSyncOverrides,
  ): Promise<void> {
    const current = (await this.options.store.readConfig(specId)) ?? {
      teamId: null,
      teamName: null,
      projectId: null,
      projectName: null,
      labelIds: [],
      labelNames: [],
    };
    await this.options.store.writeConfig(specId, {
      teamId: overrides.teamId === undefined ? current.teamId : overrides.teamId,
      teamName: overrides.teamName === undefined ? current.teamName : overrides.teamName,
      projectId: overrides.projectId === undefined ? current.projectId : overrides.projectId,
      projectName:
        overrides.projectName === undefined ? current.projectName : overrides.projectName,
      labelIds: overrides.labelIds === undefined ? current.labelIds : [...overrides.labelIds],
      labelNames:
        overrides.labelNames === undefined ? current.labelNames : [...overrides.labelNames],
    });
  }
}

/**
 * The batch key: the set of rows this sync was asked to make.
 *
 * Two clicks that ask for the same rows resume one workflow instead of racing
 * two. A click that asks for a different set is a different batch, and the
 * per-ticket ledger keeps even overlapping batches from double-creating.
 */
function batchKeyOf(ticketIds: readonly string[]): string {
  return requestHash([...ticketIds].sort()).slice(0, 32);
}
