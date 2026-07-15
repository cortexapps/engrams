/**
 * ADR 0089 run-time guardrail for abandoned session-handled tool calls.
 *
 * The scanner logs stale calls and exposes `onStale` as the deliberate hook
 * point for a future paging/alerting integration. P1 does not choose an alert
 * provider or escalation policy.
 */

import { log as rootLog } from "../log.ts";
import {
  makePendingToolCallStore,
  type PendingToolCallRow,
  type PendingToolCallStore,
} from "./pending-tool-calls.ts";

/** A human-in-the-loop call is stale after one day without submission. */
export const SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS = 24 * 60 * 60 * 1_000;

/** Pure deterministic classifier over a pending-call snapshot. */
export function findStaleSessionToolCalls(
  rows: readonly PendingToolCallRow[],
  now: () => Date = () => new Date(),
): PendingToolCallRow[] {
  const cutoffMs = now().getTime() - SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS;
  return rows.filter(
    (row) =>
      row.handling === "session" &&
      row.submittedAt == null &&
      row.requestedAt.getTime() < cutoffMs,
  );
}

export interface PendingToolCallWatchdogDeps {
  pendingCalls?: PendingToolCallStore;
  now?: () => Date;
  logStale?: (row: PendingToolCallRow, ageMs: number) => void;
  /** Future alerting hook: intentionally injectable and provider-agnostic. */
  onStale?: (rows: PendingToolCallRow[]) => Promise<void> | void;
}

/** Query, classify, log, and return stale calls without owning a scheduler. */
export async function runPendingToolCallWatchdog(
  deps: PendingToolCallWatchdogDeps = {},
): Promise<PendingToolCallRow[]> {
  const now = deps.now ?? (() => new Date());
  const current = now();
  const cutoff = new Date(current.getTime() - SESSION_TOOL_CALL_WATCHDOG_WINDOW_MS);
  const pendingCalls = deps.pendingCalls ?? makePendingToolCallStore();
  const candidates = await pendingCalls.listUnsubmittedSessionCallsBefore(cutoff);
  const stale = findStaleSessionToolCalls(candidates, () => current);
  const logStale =
    deps.logStale ??
    ((row: PendingToolCallRow, ageMs: number) => {
      rootLog.warn(
        {
          sessionId: row.sessionId,
          toolCallId: row.toolCallId,
          toolName: row.toolName,
          ageMs,
        },
        "session-handled tool call has no submitted result",
      );
    });
  for (const row of stale) {
    logStale(row, current.getTime() - row.requestedAt.getTime());
  }
  if (stale.length > 0) await deps.onStale?.(stale);
  return stale;
}
