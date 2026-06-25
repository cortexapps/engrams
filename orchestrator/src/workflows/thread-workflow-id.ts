/**
 * Thread → DBOS workflow-id selection (ADR 0060 Invariant 4). SDK-agnostic.
 *
 * A thread maps to a deterministic workflow id so the events handler's
 * `startWorkflow` is idempotent (the 1st mention creates, later ones no-op +
 * `send`). The epoch suffix handles thread REUSE: when a thread's prior
 * workflow is in a terminal DBOS state and a new mention arrives, the idempotent
 * start would otherwise swallow it — so we start a fresh successor instead.
 */

import { createHash } from "node:crypto";

/** Stable hash of a thread's identity → the workflow-id base. */
export function threadHash(team: string, channel: string, threadRoot: string): string {
  return createHash("sha256").update(`${team}:${channel}:${threadRoot}`).digest("hex").slice(0, 32);
}

/**
 * Pick the workflow id for a mention: `task:<hash>` for a fresh OR live thread;
 * `task:<hash>#<n>` (first free epoch) when prior epochs are terminal.
 * `isTerminal(id)` is true only for an EXISTING terminal workflow (false for
 * absent or live), so the first non-terminal epoch is the right target.
 */
export async function selectThreadWorkflowId(
  baseHash: string,
  isTerminal: (workflowId: string) => Promise<boolean>,
  maxEpochs = 1000,
): Promise<string> {
  for (let epoch = 0; epoch < maxEpochs; epoch++) {
    const id = epoch === 0 ? `task:${baseHash}` : `task:${baseHash}#${epoch}`;
    if (!(await isTerminal(id))) return id;
  }
  return `task:${baseHash}#${maxEpochs}`;
}
