/** Run and delivery-key identity (ADR 0119 D3). A leaf module: the store,
 * the dispatcher, and the scheduler all derive ids from here, and nothing
 * here imports back into them.
 */

export function automationRunId(automationId: string, deliveryKey: string): string {
  // The run id IS the DBOS workflow id. DBOS treats workflowID as the durable
  // execution identity: starting an existing running or terminal id returns
  // its handle and never re-executes the body.
  return `autorun:${automationId}:${deliveryKey}`;
}

/** The one place the cron delivery-key format lives. The scheduler's workflow
 * id and the store's occurrence claim must agree on it byte-for-byte. */
export function cronDeliveryKey(scheduledFor: Date): string {
  return `cron:${Math.floor(scheduledFor.getTime() / 1_000)}`;
}
