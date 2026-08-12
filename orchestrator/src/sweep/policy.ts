import { getAllRegisteredFunctions } from "../../node_modules/@dbos-inc/dbos-sdk/dist/src/decorators.js";

/**
 * Registered policies adopt; unknown names are alert-only. There are no
 * cleanup callbacks and no cancel/ignore modes — terminal failures are
 * error-level log lines (the operator wires log alerting), staleness cancels
 * and operator suppression cover every "don't adopt this" need.
 */
export interface SweepPolicy {
  mode: "adopt";
  staleAfterHours: number;
}

export const SWEEP_POLICIES: Record<string, SweepPolicy> = {
  SlackThreadWorkflow: { mode: "adopt", staleAfterHours: 48 },
  PrReviewWorkflow: { mode: "adopt", staleAfterHours: 48 },
  // Ingress resolves a pull request and hands off (ADR 0100 d11). It is bounded
  // by a couple of API calls, so an hour is generous — unlike the review pass it
  // starts, which waits on an agent and gets 48. A stranded ingress means a
  // review that was asked for and never began, so adopting it is the point.
  ReviewIngressWorkflow: { mode: "adopt", staleAfterHours: 1 },
  ToolExecWorkflow: { mode: "adopt", staleAfterHours: 1 },
  AutomationRunWorkflow: { mode: "adopt", staleAfterHours: 1 },
  // A ticket sync is a handful of Linear calls, so an hour is generous. A
  // stranded batch is rows a person asked to sync that never reached Linear,
  // and adopting it is exactly what N4 makes safe: the ledger keeps the resumed
  // batch from creating a second issue for a ticket that already has one.
  SpecTicketSyncWorkflow: { mode: "adopt", staleAfterHours: 1 },
};

export type ResolvedPolicy = SweepPolicy | { mode: "alert-only" };

export function resolvePolicy(dbName: string): ResolvedPolicy {
  const registered = SWEEP_POLICIES[dbName];
  if (registered) return registered;
  if (dbName.startsWith("temp_workflow-")) {
    return { mode: "adopt", staleAfterHours: 48 };
  }
  if (dbName.startsWith("_dbos_")) {
    return { mode: "adopt", staleAfterHours: 1 };
  }
  return { mode: "alert-only" };
}

interface RegisteredFunction {
  name: string;
  workflowConfig?: unknown;
}

export function registeredWorkflowNames(): string[] {
  const registrations: RegisteredFunction[] = getAllRegisteredFunctions();
  return registrations
    .filter((registration) => registration.workflowConfig)
    .map((registration) => registration.name);
}

export function assertSweepPoliciesExhaustive(
  names: string[] = registeredWorkflowNames(),
): void {
  const missing = names.filter(
    (name) =>
      !SWEEP_POLICIES[name] &&
      !name.startsWith("_dbos_") &&
      !name.startsWith("temp_workflow-"),
  );
  if (missing.length > 0) {
    throw new Error(
      `DBOS workflows missing sweep policies: ${missing.join(", ")}`,
    );
  }
}
