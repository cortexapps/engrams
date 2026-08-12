/**
 * The durable Linear sync batch (ADR 0114 D6, N4).
 *
 * A batch is a DBOS workflow for the same reason `tool-exec.ts` is: a pod that
 * rolls in the middle of six creates must **resume** the batch, not restart it.
 * Each ticket is its own step, so a completed step replays from its recorded
 * output and never calls Linear a second time. The ledger under
 * `syncOneSpecTicket` makes even a re-run of an unrecorded step safe, so the two
 * mechanisms cover each other: DBOS avoids the work, the ledger makes the work
 * idempotent when DBOS cannot.
 *
 * The registered body stays tiny and stable, as `tool-exec.ts` says: the
 * behavior lives in the plain functions in `../specs/ticket-sync.ts`.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { getPool } from "../db/client.ts";
import { makeLinearIssueClient } from "../integrations/linear-issues.ts";
import { log } from "../log.ts";
import {
  linkSpecTicketDependencies,
  planSpecTicketSync,
  syncOneSpecTicket,
  SpecTicketSyncError,
  type SpecTicketSyncDeps,
  type SpecTicketSyncTarget,
} from "../specs/ticket-sync.ts";
import { makeSpecTicketSyncConnector } from "../specs/ticket-sync-connector.ts";
import { PostgresSpecTicketSyncStore } from "../specs/ticket-sync-store.ts";

export interface SpecTicketSyncWorkflowInput {
  specId: string;
  ticketIds: string[];
}

/** The production wiring, resolved per step so a restart picks up a live pool. */
export function productionSpecTicketSyncDeps(): SpecTicketSyncDeps {
  return {
    store: new PostgresSpecTicketSyncStore(getPool()),
    linear: makeLinearIssueClient(),
    connector: makeSpecTicketSyncConnector(),
    log: log.child({ component: "spec-ticket-sync" }),
  };
}

/**
 * Plan the batch, or fail its rows honestly.
 *
 * The plan needs a connected Linear and a team. Both can go away between the
 * click and the driver, and a row that stays `queued` forever is the worst
 * possible answer — so the reason lands on every row the batch was asked for,
 * and the batch stops.
 */
async function planOrFail(
  input: SpecTicketSyncWorkflowInput,
): Promise<{ order: string[]; target: SpecTicketSyncTarget } | { failed: string }> {
  const deps = productionSpecTicketSyncDeps();
  try {
    const plan = await planSpecTicketSync(input, deps);
    return { order: plan.order, target: plan.target };
  } catch (error) {
    if (!(error instanceof SpecTicketSyncError)) throw error;
    for (const ticketId of input.ticketIds) {
      await deps.store.writeTicketState(input.specId, ticketId, {
        syncState: "failed",
        syncError: error.message,
      });
    }
    return { failed: error.message };
  }
}

async function specTicketSyncWorkflowEntry(input: SpecTicketSyncWorkflowInput): Promise<void> {
  const plan = await DBOS.runStep(() => planOrFail(input), { name: "spec-ticket-sync-plan" });
  if ("failed" in plan) return;
  for (const ticketId of plan.order) {
    await DBOS.runStep(
      () =>
        syncOneSpecTicket(
          { specId: input.specId, ticketId, target: plan.target },
          productionSpecTicketSyncDeps(),
        ),
      { name: "spec-ticket-sync-issue" },
    );
  }
  await DBOS.runStep(
    () => linkSpecTicketDependencies({ specId: input.specId }, productionSpecTicketSyncDeps()),
    { name: "spec-ticket-sync-relations" },
  );
}

export const specTicketSyncWorkflow = DBOS.registerWorkflow(specTicketSyncWorkflowEntry, {
  name: "SpecTicketSyncWorkflow",
});

/**
 * Start one batch under a stable id.
 *
 * The id is derived from the spec and the rows asked for, so a second click on
 * the same set attaches to the running batch instead of starting a rival one.
 */
export async function startSpecTicketSyncWorkflow(
  input: SpecTicketSyncWorkflowInput,
  workflowId: string,
): Promise<void> {
  await DBOS.startWorkflow(specTicketSyncWorkflow, { workflowID: workflowId })(input);
}
