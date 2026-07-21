/** Pull request → DBOS workflow-id selection (ADR 0100). SDK-agnostic. */

import { createHash } from "node:crypto";

export function reviewHash(repo: string, prNumber: number): string {
  return createHash("sha256")
    .update(`${repo}#${prNumber}`)
    .digest("hex")
    .slice(0, 32);
}

/** Pick the live epoch, or the first successor after terminal workflows. */
export async function selectReviewWorkflowId(
  baseHash: string,
  isTerminal: (workflowId: string) => Promise<boolean>,
  maxEpochs = 1000,
): Promise<string> {
  for (let epoch = 0; epoch < maxEpochs; epoch++) {
    const id = epoch === 0
      ? `review:${baseHash}`
      : `review:${baseHash}#${epoch}`;
    if (!(await isTerminal(id))) return id;
  }
  return `review:${baseHash}#${maxEpochs}`;
}
