/** Pure helpers for the Activity surfaces: one entry per run, in the words a
 * person uses (what happened, how it got in, how long it took). */

import type { AutomationRunBrief, FilteredWindow } from "@/gen/engram/app/v1/automation_pb";
import { triggerSourceLabel } from "../runs/run-format";

export type ActivityRow =
  | { kind: "run"; run: AutomationRunBrief }
  | { kind: "window"; window: FilteredWindow; automationId: string };

/** Interleave runs (newest first) with the filtered windows that precede each
 * run; `before_run_id === ""` pins a window after the oldest run. Windows are
 * tagged with their automation so a cross-automation ledger can name it. */
export function interleaveRuns(
  runs: readonly AutomationRunBrief[],
  windows: readonly (FilteredWindow & { automationId?: string })[],
): ActivityRow[] {
  const byBefore = new Map<string, (FilteredWindow & { automationId?: string })[]>();
  for (const window of windows) {
    const list = byBefore.get(window.beforeRunId) ?? [];
    list.push(window);
    byBefore.set(window.beforeRunId, list);
  }
  const rows: ActivityRow[] = [];
  const push = (window: FilteredWindow & { automationId?: string }) =>
    rows.push({ kind: "window", window, automationId: window.automationId ?? "" });
  for (const run of runs) {
    for (const window of byBefore.get(run.id) ?? []) push(window);
    rows.push({ kind: "run", run });
  }
  for (const window of byBefore.get("") ?? []) push(window);
  return rows;
}

export type ActivityFilter = "all" | "failed" | "running" | "superseded";

export const ACTIVE = new Set(["pending", "running", "waiting"]);

export function matchesFilter(run: AutomationRunBrief, filter: ActivityFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "failed":
      return run.status === "failed" || run.status === "deadline";
    case "running":
      return ACTIVE.has(run.status);
    case "superseded":
      return run.status === "superseded";
  }
}

export function filterCounts(runs: readonly AutomationRunBrief[]): Record<ActivityFilter, number> {
  return {
    all: runs.length,
    failed: runs.filter((r) => matchesFilter(r, "failed")).length,
    running: runs.filter((r) => matchesFilter(r, "running")).length,
    superseded: runs.filter((r) => matchesFilter(r, "superseded")).length,
  };
}

/** "pull_request.opened" → "pull request opened". */
function humanizeEvent(key: string): string {
  return key.replaceAll("_", " ").replaceAll(".", " · ");
}

/** What the entry is about, in words: the event that got in, or the kind of
 * occurrence when there was no event. A filtered delivery says so first. */
export function entryTitle(run: AutomationRunBrief): string {
  const base =
    run.triggerSource === "cron"
      ? "Scheduled run"
      : run.triggerSource === "manual"
        ? run.dryRun
          ? "Dry run"
          : "Manual run"
        : run.eventKey
          ? humanizeEvent(run.eventKey)
          : triggerSourceLabel(run.triggerSource);
  if (run.status === "filtered") return `Filtered · ${base}`;
  if (run.status === "superseded") return `Superseded · ${base}`;
  return base;
}

/** "via github · pull_request.opened", "via schedule", "via manual". */
export function entryVia(run: AutomationRunBrief): string {
  switch (run.triggerSource) {
    case "cron":
      return "via schedule";
    case "manual":
      return "via manual";
    case "integration":
    case "webhook":
      return run.eventKey
        ? `via ${run.triggerSource} · ${run.eventKey}`
        : `via ${run.triggerSource}`;
    default:
      return `via ${run.triggerSource}`;
  }
}

const HM = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

/** "14 deliveries filtered between 09:02 and 09:41 · Slack digest, PR review". */
export function windowSummary(
  windows: readonly (FilteredWindow & { automationId?: string })[],
  nameOf?: (automationId: string) => string | undefined,
): string {
  const count = windows.reduce((a, w) => a + w.count, 0);
  const first = Math.min(...windows.map((w) => Date.parse(w.firstAt)));
  const last = Math.max(...windows.map((w) => Date.parse(w.lastAt)));
  const span =
    Number.isFinite(first) && Number.isFinite(last)
      ? ` between ${HM.format(first)} and ${HM.format(last)}`
      : "";
  const names = nameOf
    ? [...new Set(windows.map((w) => (w.automationId ? nameOf(w.automationId) : undefined)))]
        .filter((n): n is string => !!n)
        .join(", ")
    : "";
  return `${count} ${count === 1 ? "delivery" : "deliveries"} filtered${span}${names ? ` · ${names}` : ""}`;
}

/** Runs created today (local), for the masthead chip. */
export function countToday(runs: readonly AutomationRunBrief[], now: number = Date.now()): number {
  const d = new Date(now);
  const start = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  return runs.filter((r) => Date.parse(r.createdAt) >= start).length;
}

/** A step error that points at fleet health gets a related-cause link. */
export function pointsAtFleet(error: string | undefined): boolean {
  return !!error && /\b(host|capacity|capabilit|no capacity|placement)\b/i.test(error);
}
