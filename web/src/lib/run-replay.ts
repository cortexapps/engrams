/** A past run folded onto the canvas: for every block the run touched, what
 * it did (status, sentence, duration) and every frame it ran in (a looped
 * block ran once per iteration). The canvas reads this by block id; nothing
 * here knows about layout or React. */

import type { AutomationStepRun } from "@/gen/engram/app/v1/automation_pb";

import type { CanvasNodeStatus } from "@/pages/automations/build/canvas/CanvasNode";
import {
  buildTimeline,
  formatDuration,
  parseJsonObject,
  type TimelineNode,
  type TimelineStep,
} from "@/pages/automations/runs/run-format";
import { stepSentence } from "./step-sentence";

export interface ReplayNode {
  blockId: string;
  /** Every frame this block ran in, in ledger order. */
  frames: TimelineStep[];
  /** The frame the canvas shows: the last one. */
  latest: TimelineStep;
  status: CanvasNodeStatus;
  sentence: string;
  duration: string;
}

export type ReplayMap = ReadonlyMap<string, ReplayNode>;

export function stepStatus(status: string): CanvasNodeStatus {
  switch (status) {
    case "succeeded":
      return "ok";
    case "failed":
    case "deadline":
      return "failed";
    case "skipped":
      return "skipped";
    default:
      return "running";
  }
}

export function sentenceFor(blockType: string | undefined, step: TimelineStep): string {
  const { latest } = step;
  return stepSentence(blockType, {
    status: latest.status,
    ...(latest.error !== undefined ? { error: latest.error } : {}),
    inputs: parseJsonObject(latest.inputsJson) ?? {},
    outputs: parseJsonObject(latest.outputsJson) ?? {},
  });
}

function flatten(nodes: readonly TimelineNode[], out: TimelineStep[] = []): TimelineStep[] {
  for (const node of nodes) {
    if (node.kind === "step") out.push(node.step);
    else flatten(node.steps, out);
  }
  return out;
}

/** Fold the step ledger by leaf block id. */
export function replayMap(
  steps: readonly AutomationStepRun[],
  blockTypeOf: (blockId: string) => string | undefined,
  now = Date.now(),
): ReplayMap {
  const map = new Map<string, ReplayNode>();
  for (const step of flatten(buildTimeline(steps))) {
    const existing = map.get(step.blockId);
    if (existing) {
      existing.frames.push(step);
      existing.latest = step;
      existing.status = stepStatus(step.latest.status);
      existing.sentence = sentenceFor(blockTypeOf(step.blockId), step);
      existing.duration = formatDuration(step.latest.startedAt, step.latest.endedAt, now);
      continue;
    }
    map.set(step.blockId, {
      blockId: step.blockId,
      frames: [step],
      latest: step,
      status: stepStatus(step.latest.status),
      sentence: sentenceFor(blockTypeOf(step.blockId), step),
      duration: formatDuration(step.latest.startedAt, step.latest.endedAt, now),
    });
  }
  return map;
}

/** Whether a way in carried this run: any of its blocks ran. A run that was
 * filtered before its first block (or has no steps yet) lights no way. */
export function wayOnPath(map: ReplayMap, blockIds: Iterable<string>): boolean {
  for (const id of blockIds) if (map.has(id)) return true;
  return false;
}
