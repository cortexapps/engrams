/** Run and delivery-key identity (ADR 0119 D3). A leaf module: the store,
 * the dispatcher, and the scheduler all derive ids from here, and nothing
 * here imports back into them.
 */

import { MAIN_ENTRYPOINT_ID } from "./engine/definition.ts";

export function automationRunId(
  automationId: string,
  deliveryKey: string,
  entrypointId: string = MAIN_ENTRYPOINT_ID,
  instanceId = "",
): string {
  // The run id IS the DBOS workflow id. DBOS treats workflowID as the durable
  // execution identity: starting an existing running or terminal id returns
  // its handle and never re-executes the body. The main entrypoint keeps the
  // historical two-part shape so pre-D9 ids never churn; an extra entrypoint
  // adds its id, because the same delivery may open one run per entrypoint.
  // An instance-bound run (ADR 0120) always spells the entrypoint and adds
  // `i-<instanceId>` — a cron occurrence fans out one workflow per open
  // instance, so the instance must be part of the durable identity.
  if (instanceId !== "") {
    return `autorun:${automationId}:${entrypointId}:i-${instanceId}:${deliveryKey}`;
  }
  return entrypointId === MAIN_ENTRYPOINT_ID
    ? `autorun:${automationId}:${deliveryKey}`
    : `autorun:${automationId}:${entrypointId}:${deliveryKey}`;
}

/** The one place the cron delivery-key format lives. The scheduler's workflow
 * id and the store's occurrence claim must agree on it byte-for-byte. */
export function cronDeliveryKey(scheduledFor: Date): string {
  return `cron:${Math.floor(scheduledFor.getTime() / 1_000)}`;
}
