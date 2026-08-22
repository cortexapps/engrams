/** Pure helpers for the runs surfaces (ADR 0119 phase 3.7). */

import type { AutomationStepRun } from "@/gen/engram/app/v1/automation_pb";

export const ACTIVE_RUN_STATUSES = new Set(["pending", "running", "waiting"]);

export function isActiveRunStatus(status: string): boolean {
  return ACTIVE_RUN_STATUSES.has(status);
}

export function runStatusLabel(status: string): string {
  return status.replaceAll("_", " ");
}

export function triggerSourceLabel(source: string, eventKey?: string): string {
  switch (source) {
    case "cron":
      return "Schedule";
    case "manual":
      return "Manual";
    case "integration":
      return eventKey ? `Event · ${eventKey}` : "Event";
    case "webhook":
      return eventKey ? `Webhook · ${eventKey}` : "Webhook";
    default:
      return source;
  }
}

export function formatDuration(startedAt?: string, endedAt?: string, now = Date.now()): string {
  if (!startedAt) return "—";
  const start = Date.parse(startedAt);
  if (Number.isNaN(start)) return "—";
  const end = endedAt ? Date.parse(endedAt) : now;
  const ms = Math.max(0, end - start);
  if (ms < 1_000) return `${ms}ms`;
  const s = Math.round(ms / 1_000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

// ---------------------------------------------------------------------------
// Frame paths → timeline tree
// ---------------------------------------------------------------------------

/** One segment of a frame path: `poll[2]` → { blockId: "poll", iteration: 2 }. */
export interface Frame {
  blockId: string;
  iteration?: number;
}

const SEGMENT_RE = /^([a-z][a-z0-9_]*)(?:\[(\d+)\])?$/;

/** Parse the engine's frame path (`loop[1].child`, `gate.hot_path`,
 * `x.__cond__`, `loop[0].__until__`). Unknown shapes fall back to one
 * opaque frame so a row is never lost. */
export function parseFramePath(path: string): Frame[] {
  const frames: Frame[] = [];
  for (const segment of path.split(".")) {
    const m = SEGMENT_RE.exec(segment);
    if (!m) return [{ blockId: path }];
    frames.push(
      m[2] !== undefined ? { blockId: m[1]!, iteration: Number(m[2]) } : { blockId: m[1]! },
    );
  }
  return frames;
}

/** Ledger rows the engine writes for its own bookkeeping; the timeline shows
 * them as the block they belong to, not as separate rows. */
export function isAuxiliaryStep(blockId: string): boolean {
  return blockId.startsWith("__") || blockId.endsWith("__cond__") || blockId.endsWith("__until__");
}

export interface TimelineStep {
  /** Full frame path (the ledger's block_id). */
  path: string;
  /** Leaf block id. */
  blockId: string;
  /** Nesting depth (0 = top level). */
  depth: number;
  /** Highest attempt seen; `attempts` holds every ledger row for the step. */
  attempt: number;
  attempts: AutomationStepRun[];
  latest: AutomationStepRun;
}

export interface TimelineGroup {
  kind: "iteration";
  loopId: string;
  iteration: number;
  depth: number;
  steps: TimelineNode[];
}

export type TimelineNode = { kind: "step"; step: TimelineStep } | TimelineGroup;

/** Fold the flat step ledger into a tree: loop iterations group their body
 * steps; nesting depth comes from the frame path; retries collapse into one
 * row carrying every attempt. Order follows first appearance (the ledger is
 * written in walk order). */
export function buildTimeline(steps: readonly AutomationStepRun[]): TimelineNode[] {
  const roots: TimelineNode[] = [];
  const byPath = new Map<string, TimelineStep>();
  const groups = new Map<string, TimelineGroup>();

  const containerFor = (frames: Frame[]): TimelineNode[] => {
    // Walk every iteration frame except the leaf; each opens/finds a group.
    let container = roots;
    let depth = 0;
    let prefix = "";
    for (let i = 0; i < frames.length - 1; i += 1) {
      const frame = frames[i]!;
      prefix = prefix ? `${prefix}.${frame.blockId}` : frame.blockId;
      depth += 1;
      if (frame.iteration === undefined) continue;
      const key = `${prefix}[${frame.iteration}]`;
      let group = groups.get(key);
      if (!group) {
        group = {
          kind: "iteration",
          loopId: frame.blockId,
          iteration: frame.iteration,
          depth: depth - 1,
          steps: [],
        };
        groups.set(key, group);
        container.push(group);
      }
      container = group.steps;
      prefix = key;
    }
    return container;
  };

  for (const row of steps) {
    if (isAuxiliaryStep(row.blockId.split(".").at(-1) ?? row.blockId)) continue;
    const existing = byPath.get(row.blockId);
    if (existing) {
      existing.attempts.push(row);
      if (row.attempt >= existing.attempt) {
        existing.attempt = row.attempt;
        existing.latest = row;
      }
      continue;
    }
    const frames = parseFramePath(row.blockId);
    const container = containerFor(frames);
    const step: TimelineStep = {
      path: row.blockId,
      blockId: frames.at(-1)!.blockId,
      depth: frames.length - 1,
      attempt: row.attempt,
      attempts: [row],
      latest: row,
    };
    byPath.set(row.blockId, step);
    container.push({ kind: "step", step });
  }
  return roots;
}

/** Locate a step by its stable frame path in a (freshly built) tree. The run
 * page keys its selection on the path, never on a step object, so an open
 * drawer follows each poll instead of freezing at click time. */
export function findTimelineStep(
  nodes: readonly TimelineNode[],
  path: string,
): TimelineStep | null {
  for (const node of nodes) {
    if (node.kind === "step") {
      if (node.step.path === path) return node.step;
      continue;
    }
    const found = findTimelineStep(node.steps, path);
    if (found) return found;
  }
  return null;
}

export function parseJsonObject(text: string): Record<string, unknown> | null {
  if (!text) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}
