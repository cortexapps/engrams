/** Verified webhook occurrence routing into AutomationRunWorkflow. */

import { DBOS } from "@dbos-inc/dbos-sdk";

import {
  makeAutomationStore,
  type AutomationRow,
  type WebhookRegistrationRow,
} from "../db/automations.ts";
import { automationRunWorkflow, type AutomationRunWorkflowInput } from "../workflows/automation-run.ts";
import { matchesWebhookFilter, SYSTEM_GITHUB_REGISTRATION_ID } from "./webhook.ts";

export { SYSTEM_GITHUB_REGISTRATION_ID } from "./webhook.ts";
export const WEBHOOK_SAMPLE_RETENTION = 20;

export interface AutomationWebhookStore {
  recordWebhookSample(input: {
    registrationId: string;
    eventKey: string;
    payload: Record<string, unknown>;
    receivedAt: Date;
    retain: number;
  }): Promise<void>;
  listEnabledForWebhookRegistration(registrationId: string): Promise<AutomationRow[]>;
}

export interface AutomationWebhookStarter {
  start(input: AutomationRunWorkflowInput, workflowId: string): Promise<void>;
}

export interface DispatchWebhookInput {
  registrationId: string;
  /** Null for well-known system registrations that are not persisted in PG. */
  registration: WebhookRegistrationRow | null;
  eventKey: string;
  deliveryId: string;
  payload: Record<string, unknown>;
  receivedAt: Date;
}

export interface DispatchWebhookResult {
  matched: number;
  started: number;
}

export interface DispatchWebhookDeps {
  store?: AutomationWebhookStore;
  workflowStarter?: AutomationWebhookStarter;
  randomUUID?: () => string;
}

export function automationWebhookWorkflowId(
  automationId: string,
  deliveryId: string,
): string {
  return `auto:${automationId}:${deliveryId}`;
}

export async function dispatchWebhookOccurrence(
  input: DispatchWebhookInput,
  deps: DispatchWebhookDeps = {},
): Promise<DispatchWebhookResult> {
  const store = deps.store ?? makeAutomationStore();
  const randomUUID = deps.randomUUID ?? (() => crypto.randomUUID());
  const starter = deps.workflowStarter ?? {
    async start(workflowInput: AutomationRunWorkflowInput, workflowId: string) {
      // DBOS treats workflowID as the durable execution identity. Starting an
      // existing running or terminal id returns its existing handle/result and
      // never executes this fresh input's body.
      await DBOS.startWorkflow(automationRunWorkflow, { workflowID: workflowId })(workflowInput);
    },
  };

  if (input.registration !== null) {
    await store.recordWebhookSample({
      registrationId: input.registrationId,
      eventKey: input.eventKey,
      payload: input.payload,
      receivedAt: input.receivedAt,
      retain: WEBHOOK_SAMPLE_RETENTION,
    });
  }

  const matches = (await store.listEnabledForWebhookRegistration(input.registrationId))
    .filter((automation) => {
      if (automation.trigger.kind !== "webhook") return false;
      return automation.trigger.events.includes(input.eventKey)
        && matchesWebhookFilter(input.payload, automation.trigger.filter);
    });

  await Promise.all(matches.map(async (automation) => {
    const workflowInput: AutomationRunWorkflowInput = {
      automationId: automation.id,
      runId: randomUUID(),
      trigger: {
        source: "webhook",
        eventKey: input.eventKey,
        deliveryId: input.deliveryId,
        payload: input.payload,
      },
      receivedAt: input.receivedAt.toISOString(),
    };
    await starter.start(
      workflowInput,
      automationWebhookWorkflowId(automation.id, input.deliveryId),
    );
  }));

  return { matched: matches.length, started: matches.length };
}
