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
import type { AutomationRunTrigger, AutomationTrigger } from "../db/schema.ts";
import type { IntegrationEventDispatchInput } from "./integration-ingress.ts";
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

// ---------------------------------------------------------------------------
// Integration triggers (ADR 0119 D5, stack item 2.C)
// ---------------------------------------------------------------------------

export type IntegrationTriggerSpec = Extract<AutomationTrigger, { kind: "integration" }>;

export interface IntegrationDispatchStore extends AutomationDispatchStore {
  listEnabledForIntegrationTrigger(
    provider: string,
    connectionId: string,
  ): Promise<DispatchTarget[]>;
}

export interface DispatchIntegrationDeps {
  store?: IntegrationDispatchStore;
  workflowStarter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  now?: () => Date;
}

export interface DispatchIntegrationResult {
  matched: number;
  started: number;
  joined: number;
  queued: number;
  skipped: number;
}

/** Providers whose scope noun compares case-insensitively (GitHub owner/repo).
 * Slack channel ids and Linear team keys are exact. */
const CASE_INSENSITIVE_SCOPE_PROVIDERS = new Set(["github"]);

function normalizeScope(provider: string, value: string): string {
  return CASE_INSENSITIVE_SCOPE_PROVIDERS.has(provider) ? value.toLowerCase() : value;
}

/** Resolve a {fromInput} scope binding against the automation's input values:
 * a map-typed input contributes its keys, a list input its string elements.
 * Anything else (absent, wrong shape) resolves to undefined → no match. */
export function scopeValuesFromInput(
  inputs: Record<string, unknown>,
  key: string,
): string[] | undefined {
  if (!Object.prototype.hasOwnProperty.call(inputs, key)) return undefined;
  const value = inputs[key];
  if (Array.isArray(value)) {
    const strings = value.filter((item): item is string => typeof item === "string");
    return strings.length === value.length ? strings : undefined;
  }
  if (typeof value === "object" && value !== null) {
    return Object.keys(value);
  }
  return undefined;
}

export function matchesIntegrationTrigger(
  trigger: IntegrationTriggerSpec,
  event: { provider: string; connectionId: string; eventKey: string; scopeValue?: string },
  resolveInput: (key: string) => string[] | undefined,
): boolean {
  if (trigger.provider !== event.provider) return false;
  if (trigger.connectionId !== event.connectionId) return false;
  if (!trigger.eventKeys.includes(event.eventKey)) return false;
  if (trigger.scope === undefined) return true;
  const values =
    "values" in trigger.scope ? trigger.scope.values : resolveInput(trigger.scope.fromInput);
  // An unresolved input binding narrows to nothing: never fire on a scope the
  // author has not spelled out.
  if (values === undefined || event.scopeValue === undefined) return false;
  const scoped = normalizeScope(event.provider, event.scopeValue);
  return values.some((value) => normalizeScope(event.provider, value) === scoped);
}

/** Route one verified, ledgered integration delivery to every enabled
 * automation whose trigger matches, through the shared admission path.
 * Installed onto the ingress spine's dispatch seam at boot. */
export async function dispatchIntegrationEvent(
  input: IntegrationEventDispatchInput,
  deps: DispatchIntegrationDeps = {},
): Promise<DispatchIntegrationResult> {
  const store = deps.store ?? makeAutomationStore();
  const starter = deps.workflowStarter ?? defaultWorkflowStarter();
  const sender = deps.sender ?? defaultAutomationSender;
  const now = deps.now ?? (() => new Date());

  const targets = (
    await store.listEnabledForIntegrationTrigger(input.provider, input.connectionId)
  ).filter((target) => {
    const trigger = target.definition.trigger;
    if (trigger.kind !== "integration") return false;
    return matchesIntegrationTrigger(
      trigger,
      {
        provider: input.provider,
        connectionId: input.connectionId,
        eventKey: input.eventKey,
        ...(input.scopeValue !== undefined ? { scopeValue: input.scopeValue } : {}),
      },
      (key) => scopeValuesFromInput(target.automation.inputs, key),
    );
  });

  const result: DispatchIntegrationResult = {
    matched: targets.length,
    started: 0,
    joined: 0,
    queued: 0,
    skipped: 0,
  };

  for (const target of targets) {
    const deliveryKey = `${input.provider}:${input.deliveryId}`;
    const outcome = await admitAutomationRun(
      {
        target,
        runId: automationRunId(target.automation.id, deliveryKey),
        deliveryKey,
        trigger: {
          source: "integration",
          eventKey: input.eventKey,
          deliveryId: input.deliveryId,
          payload: input.payload,
          receivedAt: input.receivedAt.toISOString(),
          ...(input.scopeValue !== undefined ? { scopeValue: input.scopeValue } : {}),
        },
        scheduledFor: null,
      },
      { store, starter, sender, now },
    );
    result[outcome] += 1;
  }

  return result;
}
