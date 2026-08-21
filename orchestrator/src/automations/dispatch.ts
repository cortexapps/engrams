/** Verified webhook occurrence routing into the interpreter workflow
 * (ADR 0102 ingress → ADR 0119 admission).
 *
 * Admission (concurrency policy) is decided here, before any workflow starts:
 * queue leaves a pending run for the holder's finalize to promote; supersede
 * CASes the claim and signals the old run; skip records an auditable filtered
 * run; join routes the delivery into the active run's mailbox.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import {
  makeAutomationStore,
  type AutomationDispatchStore,
  type DispatchTarget,
  type WebhookRegistrationRow,
} from "../db/automations.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";
import { renderAutomationTemplateInScope } from "./template.ts";
import {
  defaultAutomationSender,
  inboxKeys,
  type AutomationSender,
} from "./engine/inbox.ts";
import { automationRunWorkflow, type AutomationRunWorkflowInput } from "../workflows/automation-run.ts";
import { matchesWebhookFilter, SYSTEM_GITHUB_REGISTRATION_ID } from "./webhook.ts";

export { SYSTEM_GITHUB_REGISTRATION_ID } from "./webhook.ts";
export const WEBHOOK_SAMPLE_RETENTION = 20;

export interface AutomationWebhookStore extends AutomationDispatchStore {
  recordWebhookSample(input: {
    registrationId: string;
    eventKey: string;
    payload: Record<string, unknown>;
    receivedAt: Date;
    retain: number;
  }): Promise<void>;
  listEnabledForWebhookRegistration(registrationId: string): Promise<DispatchTarget[]>;
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
  joined: number;
  queued: number;
  skipped: number;
}

export interface DispatchWebhookDeps {
  store?: AutomationWebhookStore;
  workflowStarter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  now?: () => Date;
}

import { automationRunId } from "./ids.ts";

export { automationRunId, cronDeliveryKey } from "./ids.ts";

export function defaultWorkflowStarter(): AutomationWebhookStarter {
  return {
    async start(workflowInput, workflowId) {
      await DBOS.startWorkflow(automationRunWorkflow, { workflowID: workflowId })(workflowInput);
    },
  };
}

export interface AdmitRunInput {
  target: DispatchTarget;
  runId: string;
  deliveryKey: string;
  trigger: AutomationRunTrigger;
  scheduledFor: Date | null;
}

export type AdmitOutcome = "started" | "joined" | "queued" | "skipped";

/** Decide admission for one matched occurrence and, unless the policy says
 * otherwise, create the run row and start its workflow. Shared by the webhook
 * dispatcher and the cron scheduler. */
export async function admitAutomationRun(
  input: AdmitRunInput,
  deps: {
    store: AutomationDispatchStore;
    starter: AutomationWebhookStarter;
    sender: AutomationSender;
    now: () => Date;
  },
): Promise<AdmitOutcome> {
  const { target, runId, deliveryKey, trigger } = input;
  const automationId = target.automation.id;
  const concurrency = target.definition.settings.concurrency;

  const startRun = async (concurrencyKey: string | null): Promise<void> => {
    await deps.store.insertRun({
      id: runId,
      automationId,
      version: target.automation.currentVersion,
      trigger,
      deliveryKey,
      concurrencyKey,
      scheduledFor: input.scheduledFor,
    });
    await deps.starter.start({ runId, automationId }, runId);
  };

  if (!concurrency) {
    await startRun(null);
    return "started";
  }

  // The key renders from trigger/event/inputs only (no steps exist yet).
  const key = await renderAutomationTemplateInScope(concurrency.keyTemplate, {
    trigger: {
      kind: trigger.source,
      ...(trigger.eventKey !== undefined ? { event: trigger.eventKey } : {}),
      ...(trigger.receivedAt !== undefined ? { received_at: trigger.receivedAt } : {}),
    },
    event: { raw: trigger.payload ?? {} },
    inputs: target.automation.inputs,
  });

  const claim = await deps.store.claimConcurrency(automationId, key, runId);
  if (claim.claimed) {
    await startRun(key);
    return "started";
  }

  switch (concurrency.policy) {
    case "join": {
      // No run row: the delivery joins the holder's mailbox.
      await deps.sender.send(
        claim.holderRunId,
        {
          kind: "event",
          eventKey: trigger.eventKey ?? trigger.source,
          deliveryKey,
          payload: trigger.payload ?? {},
          receivedAt: trigger.receivedAt ?? deps.now().toISOString(),
        },
        inboxKeys.joinedEvent(deliveryKey, claim.holderRunId),
      );
      return "joined";
    }
    case "queue": {
      await deps.store.insertRun({
        id: runId,
        automationId,
        version: target.automation.currentVersion,
        trigger,
        deliveryKey,
        concurrencyKey: key,
        scheduledFor: input.scheduledFor,
        // Stays pending; the holder's finalize promotes and starts it.
      });
      return "queued";
    }
    case "skip": {
      await deps.store.insertRun({
        id: runId,
        automationId,
        version: target.automation.currentVersion,
        trigger,
        deliveryKey,
        concurrencyKey: key,
        scheduledFor: input.scheduledFor,
        status: "filtered",
        error: `concurrency: skipped (active run ${claim.holderRunId})`,
        endedAt: deps.now(),
      });
      return "skipped";
    }
    case "supersede": {
      const won = await deps.store.casConcurrency(automationId, key, claim.holderRunId, runId);
      if (!won) {
        // A concurrent superseder took the claim first; this delivery loses.
        await deps.store.insertRun({
          id: runId,
          automationId,
          version: target.automation.currentVersion,
          trigger,
          deliveryKey,
          concurrencyKey: key,
          scheduledFor: input.scheduledFor,
          status: "filtered",
          error: "concurrency: superseded before start",
          endedAt: deps.now(),
        });
        return "skipped";
      }
      await deps.sender.send(
        claim.holderRunId,
        { kind: "supersede", byRunId: runId },
        inboxKeys.supersede(claim.holderRunId, runId),
      );
      await startRun(key);
      return "started";
    }
  }
}

export async function dispatchWebhookOccurrence(
  input: DispatchWebhookInput,
  deps: DispatchWebhookDeps = {},
): Promise<DispatchWebhookResult> {
  const store = deps.store ?? makeAutomationStore();
  const starter = deps.workflowStarter ?? defaultWorkflowStarter();
  const sender = deps.sender ?? defaultAutomationSender;
  const now = deps.now ?? (() => new Date());

  if (input.registration !== null) {
    await store.recordWebhookSample({
      registrationId: input.registrationId,
      eventKey: input.eventKey,
      payload: input.payload,
      receivedAt: input.receivedAt,
      retain: WEBHOOK_SAMPLE_RETENTION,
    });
  }

  const targets = (await store.listEnabledForWebhookRegistration(input.registrationId)).filter(
    (target) => {
      const trigger = target.definition.trigger;
      if (trigger.kind !== "webhook") return false;
      return (
        trigger.events.includes(input.eventKey)
        && matchesWebhookFilter(input.payload, trigger.filter)
      );
    },
  );

  const result: DispatchWebhookResult = {
    matched: targets.length,
    started: 0,
    joined: 0,
    queued: 0,
    skipped: 0,
  };

  for (const target of targets) {
    const deliveryKey = `webhook:${input.deliveryId}`;
    const outcome = await admitAutomationRun(
      {
        target,
        runId: automationRunId(target.automation.id, deliveryKey),
        deliveryKey,
        trigger: {
          source: "webhook",
          eventKey: input.eventKey,
          deliveryId: input.deliveryId,
          payload: input.payload,
          receivedAt: input.receivedAt.toISOString(),
        },
        scheduledFor: null,
      },
      { store, starter, sender, now },
    );
    result[outcome] += 1;
  }

  return result;
}
