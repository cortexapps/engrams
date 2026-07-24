import { getAllRegisteredFunctions } from "../../node_modules/@dbos-inc/dbos-sdk/dist/src/decorators.js";
import type { Logger } from "pino";

import type { SweepLookupStore } from "../db/dbos-sweep.ts";
import { failReviewCleanup, notifyThread } from "./cleanups.ts";

export type SweepMode = "adopt" | "cancel" | "ignore";

export interface FailedWorkflow {
  workflowUuid: string;
  name: string;
  status: string;
  updatedAtEpochMs: number;
}

export interface SweepContext {
  log: Logger;
  lookups: SweepLookupStore;
  slack: () => Promise<SlackPostClient>;
  failReview: (
    reviewId: string,
    opts: { reason?: string },
  ) => Promise<void>;
}

export interface SlackPostClient {
  chat: {
    postMessage(args: {
      channel: string;
      thread_ts?: string;
      text?: string;
    }): Promise<unknown>;
  };
}

export interface SweepPolicy {
  mode: SweepMode;
  staleAfterHours: number;
  onTerminalFailure?: (
    ctx: SweepContext,
    wf: FailedWorkflow,
  ) => Promise<void>;
}

export const SWEEP_POLICIES: Record<string, SweepPolicy> = {
  SlackThreadWorkflow: {
    mode: "adopt",
    staleAfterHours: 48,
    onTerminalFailure: notifyThread,
  },
  PrReviewWorkflow: {
    mode: "adopt",
    staleAfterHours: 48,
    onTerminalFailure: failReviewCleanup,
  },
  ToolExecWorkflow: {
    mode: "adopt",
    staleAfterHours: 1,
  },
  AutomationRunWorkflow: {
    mode: "adopt",
    staleAfterHours: 1,
  },
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
