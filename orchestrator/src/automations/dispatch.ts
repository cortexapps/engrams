/** Verified webhook occurrence routing into the interpreter workflow
 * (ADR 0102 ingress → ADR 0119 admission).
 *
 * Admission (concurrency policy) is decided here, before any workflow starts:
 * queue leaves a pending run for the holder's finalize to promote; supersede
 * CASes the claim and signals the old run; skip records an auditable filtered
 * run; join routes the delivery into the active run's mailbox.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { config } from "../config.ts";
import { log as rootLog } from "../log.ts";

import {
  makeAutomationStore,
  type AutomationDispatchStore,
  type DispatchTarget,
  type WebhookRegistrationRow,
} from "../db/automations.ts";
import type { AutomationRunTrigger, AutomationTrigger } from "../db/schema.ts";
import type { IntegrationEventDispatchInput } from "./integration-ingress.ts";
import { CASE_INSENSITIVE_HANDLE_PROVIDERS } from "./handles.ts";
import {
  instanceConcurrencyKey,
  resolveInstance,
  type InstanceResolution,
} from "./instances.ts";
import {
  makeAutomationInstanceStore,
  type AutomationInstanceStore,
} from "../db/automation-instances.ts";
import { loadRegistry, type WebhookFacet } from "../connectors/registry.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { getDb } from "../db/client.ts";
import { renderAutomationTemplateInScope } from "./template.ts";
import {
  defaultAutomationSender,
  inboxKeys,
  type AutomationSender,
} from "./engine/inbox.ts";
import {
  entrypointsOf,
  MAIN_ENTRYPOINT_ID,
  type AutomationEntrypoint,
  type TriggerSpec,
} from "./engine/definition.ts";
import { automationRunWorkflow, type AutomationRunWorkflowInput } from "../workflows/automation-run.ts";
import { matchesWebhookFilter } from "./webhook.ts";

export const WEBHOOK_SAMPLE_RETENTION = 20;

const log = rootLog.child({ component: "automation-dispatch" });

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
  registration: WebhookRegistrationRow;
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
  /** ADR 0120: instance-admission drops (no run row; audited in the ring). */
  dropped: number;
}

export interface DispatchWebhookDeps {
  store?: AutomationWebhookStore;
  workflowStarter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  now?: () => Date;
  /** ADR 0120 instances. */
  instances?: AutomationInstanceStore;
  facets?: (provider: string) => Promise<Pick<WebhookFacet, "events"> | undefined>;
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
  /** D9: the matched entrypoint. Default: the main entrypoint. Admission
   * reads the TRIGGER from here (continueOnly lives per entrypoint) and
   * stamps the id on the run row. */
  entrypoint?: { id: string; trigger: TriggerSpec };
  /** ADR 0120: the bound workstream ('' = unbound). Stamped on the run row
   * and prefixed onto the concurrency key, so claims isolate per instance. */
  instanceId?: string;
  trigger: AutomationRunTrigger;
  scheduledFor: Date | null;
  /** Editor DryRun: the run row is flagged and integration actions stub. */
  dryRun?: boolean;
}

export type AdmitOutcome = "started" | "joined" | "queued" | "skipped";

/** The concurrency key renders from trigger/event/inputs only — no steps
 * exist before admission. Shared by webhook/integration dispatch and the cron
 * scheduler. */
export async function renderConcurrencyKey(
  keyTemplate: string,
  target: DispatchTarget,
  trigger: AutomationRunTrigger,
): Promise<string> {
  return renderAutomationTemplateInScope(keyTemplate, {
    trigger: {
      kind: trigger.source,
      ...(trigger.eventKey !== undefined ? { event: trigger.eventKey } : {}),
      ...(trigger.receivedAt !== undefined ? { received_at: trigger.receivedAt } : {}),
    },
    event: { raw: trigger.payload ?? {} },
    inputs: target.automation.inputs,
  });
}

/** Cron admission (debt ledger 3.1): the occurrence claim already created
 * the run row, so this applies ONLY the concurrency policy to an existing
 * pending run. Returns the outcome; "started" means the caller should start
 * the workflow, anything else means the row was settled here. */
export async function admitClaimedCronRun(
  input: {
    target: DispatchTarget;
    run: { id: string; deliveryKey: string | null; instanceId?: string };
    trigger: AutomationRunTrigger;
  },
  deps: {
    store: Pick<AutomationDispatchStore, "claimConcurrency" | "casConcurrency"> & {
      settleRunConcurrency(
        runId: string,
        concurrencyKey: string,
        terminal?: { status: "filtered"; error: string },
      ): Promise<void>;
    };
    sender: AutomationSender;
    now: () => Date;
  },
): Promise<AdmitOutcome> {
  const { target, run, trigger } = input;
  const concurrency = target.definition.settings.concurrency;
  if (!concurrency) return "started";
  const automationId = target.automation.id;
  const key = instanceConcurrencyKey(
    run.instanceId ?? "",
    await renderConcurrencyKey(concurrency.keyTemplate, target, trigger),
  );
  const claim = await deps.store.claimConcurrency(automationId, key, run.id);
  if (claim.claimed) {
    await deps.store.settleRunConcurrency(run.id, key);
    return "started";
  }
  const deliveryKey = run.deliveryKey ?? run.id;
  switch (concurrency.policy) {
    case "join":
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
      await deps.store.settleRunConcurrency(run.id, key, {
        status: "filtered",
        error: `concurrency: joined active run ${claim.holderRunId}`,
      });
      return "joined";
    case "queue":
      await deps.store.settleRunConcurrency(run.id, key);
      return "queued";
    case "skip":
      await deps.store.settleRunConcurrency(run.id, key, {
        status: "filtered",
        error: `concurrency: skipped (active run ${claim.holderRunId})`,
      });
      return "skipped";
    case "supersede": {
      const won = await deps.store.casConcurrency(automationId, key, claim.holderRunId, run.id);
      if (!won) {
        await deps.store.settleRunConcurrency(run.id, key, {
          status: "filtered",
          error: "concurrency: superseded before start",
        });
        return "skipped";
      }
      await deps.sender.send(
        claim.holderRunId,
        { kind: "supersede", byRunId: run.id },
        inboxKeys.supersede(claim.holderRunId, run.id),
      );
      await deps.store.settleRunConcurrency(run.id, key);
      return "started";
    }
  }
}

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
  const instanceId = input.instanceId ?? "";
  const concurrency = target.definition.settings.concurrency;
  const entrypoint = input.entrypoint ?? {
    id: MAIN_ENTRYPOINT_ID,
    trigger: target.definition.trigger,
  };

  const startRun = async (concurrencyKey: string | null): Promise<void> => {
    await deps.store.insertRun({
      id: runId,
      automationId,
      version: target.automation.currentVersion,
      entrypointId: entrypoint.id,
      ...(instanceId !== "" ? { instanceId } : {}),
      trigger,
      deliveryKey,
      concurrencyKey,
      scheduledFor: input.scheduledFor,
      ...(input.dryRun ? { dryRun: true } : {}),
    });
    await deps.starter.start({ runId, automationId }, runId);
  };

  if (!concurrency) {
    await startRun(null);
    return "started";
  }

  const key = instanceConcurrencyKey(
    instanceId,
    await renderConcurrencyKey(concurrency.keyTemplate, target, trigger),
  );

  const joinHolder = async (holderRunId: string): Promise<"joined"> => {
    // No run row: the delivery joins the holder's mailbox.
    await deps.sender.send(
      holderRunId,
      {
        kind: "event",
        eventKey: trigger.eventKey ?? trigger.source,
        deliveryKey,
        payload: trigger.payload ?? {},
        receivedAt: trigger.receivedAt ?? deps.now().toISOString(),
      },
      inboxKeys.joinedEvent(deliveryKey, holderRunId),
    );
    return "joined";
  };

  // A continue-only event (trigger.continueOnly) belongs to an ACTIVE run or
  // to nobody: it never claims the key, so it can never open a run. This is
  // the one place that invariant lives — a Slack thread reply in a flagged
  // channel continues the thread the bot was mentioned in, and a reply in
  // any other thread is dropped here without a run row (validation pins
  // continueOnly to policy join).
  const triggerSpec = entrypoint.trigger;
  if (
    triggerSpec.kind === "integration" &&
    trigger.eventKey !== undefined &&
    triggerSpec.continueOnly?.includes(trigger.eventKey)
  ) {
    const holder = await deps.store.getConcurrencyHolder(automationId, key);
    return holder === null ? "skipped" : joinHolder(holder);
  }

  const claim = await deps.store.claimConcurrency(automationId, key, runId);
  if (claim.claimed) {
    await startRun(key);
    return "started";
  }

  switch (concurrency.policy) {
    case "join":
      return joinHolder(claim.holderRunId);
    case "queue": {
      await deps.store.insertRun({
        id: runId,
        automationId,
        version: target.automation.currentVersion,
        entrypointId: entrypoint.id,
        ...(instanceId !== "" ? { instanceId } : {}),
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
        entrypointId: entrypoint.id,
        ...(instanceId !== "" ? { instanceId } : {}),
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
          entrypointId: entrypoint.id,
          ...(instanceId !== "" ? { instanceId } : {}),
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

/** Lazy default instance store: constructed on FIRST USE, so a dispatch
 * with no instanced targets (every pre-instance automation, every unit
 * test) never touches getDb(). */
export function defaultInstanceStoreLazy(): AutomationInstanceStore {
  let inner: AutomationInstanceStore | undefined;
  const get = (): AutomationInstanceStore => (inner ??= makeAutomationInstanceStore());
  return {
    openInstance: (input) => get().openInstance(input),
    getInstance: (id) => get().getInstance(id),
    getOpenInstanceByKey: (automationId, key) => get().getOpenInstanceByKey(automationId, key),
    listOpenInstances: (automationId, limit) => get().listOpenInstances(automationId, limit),
    closeInstance: (input) => get().closeInstance(input),
    recordInstanceHandle: (input) => get().recordInstanceHandle(input),
    resolveHandles: (automationId, handles) => get().resolveHandles(automationId, handles),
    recordDrop: (input) => get().recordDrop(input),
    listRecentDrops: (automationId, limit) => get().listRecentDrops(automationId, limit),
    listInstances: (automationId, opts) => get().listInstances(automationId, opts),
    listInstanceHandles: (instanceId) => get().listInstanceHandles(instanceId),
  };
}

/** Default facet resolver: the connector registry's webhook facet for a
 * provider (handle-candidate declarations live there). */
export function defaultWebhookFacetResolver(): (
  provider: string,
) => Promise<Pick<WebhookFacet, "events"> | undefined> {
  let connectors: ReturnType<typeof makeConnectorStore> | undefined;
  return async (provider) => {
    connectors ??= makeConnectorStore(getDb());
    return (await loadRegistry(connectors)).get(provider)?.webhook;
  };
}

/** Settle one (target, entrypoint) resolution before admission: record a
 * drop (best-effort — audit must never fail the delivery), open the
 * instance when the resolution asks for it, and hand back the bound id. */
async function settleInstanceResolution(
  resolution: InstanceResolution,
  ctx: {
    automationId: string;
    entrypointId: string;
    eventKey: string;
    deliveryKey: string;
    instances: AutomationInstanceStore;
  },
): Promise<{ instanceId: string } | "dropped"> {
  switch (resolution.kind) {
    case "none":
      return { instanceId: "" };
    case "bound":
      return { instanceId: resolution.instance.id };
    case "open": {
      const instance = await ctx.instances.openInstance({
        automationId: ctx.automationId,
        key: resolution.key,
        inputs: resolution.inputs,
        openedBy: `event:${ctx.deliveryKey}`,
      });
      return { instanceId: instance.id };
    }
    case "drop": {
      try {
        await ctx.instances.recordDrop({
          automationId: ctx.automationId,
          entrypointId: ctx.entrypointId,
          eventKey: ctx.eventKey,
          reason: resolution.reason,
          detail: resolution.detail,
        });
      } catch (error) {
        log.warn(
          { automationId: ctx.automationId, reason: resolution.reason, error },
          "instance admission: drop audit write failed",
        );
      }
      log.info(
        {
          automationId: ctx.automationId,
          entrypointId: ctx.entrypointId,
          eventKey: ctx.eventKey,
          reason: resolution.reason,
          detail: resolution.detail,
        },
        "instance admission: event dropped",
      );
      return "dropped";
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
  const instances = deps.instances ?? defaultInstanceStoreLazy();
  const facets = deps.facets ?? defaultWebhookFacetResolver();

  await store.recordWebhookSample({
    registrationId: input.registrationId,
    eventKey: input.eventKey,
    payload: input.payload,
    receivedAt: input.receivedAt,
    retain: WEBHOOK_SAMPLE_RETENTION,
  });

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
    dropped: 0,
  };

  const providerHint = input.registration.providerHint;
  // The facet is only consulted for handle candidates, which only instanced
  // automations use — never load the registry for a pre-instance dispatch.
  const anyInstanced = targets.some((t) => t.definition.settings.instance !== undefined);
  const facet =
    anyInstanced && providerHint !== null ? await facets(providerHint) : undefined;

  for (const target of targets) {
    const deliveryKey = `webhook:${input.deliveryId}`;
    const trigger: AutomationRunTrigger = {
      source: "webhook",
      eventKey: input.eventKey,
      deliveryId: input.deliveryId,
      payload: input.payload,
      receivedAt: input.receivedAt.toISOString(),
    };
    const entrypoint = { id: MAIN_ENTRYPOINT_ID, trigger: target.definition.trigger };
    const resolution = await resolveInstance(
      {
        target,
        entrypoint,
        trigger,
        ...(providerHint !== null ? { provider: providerHint } : {}),
        ...(facet !== undefined ? { facet } : {}),
      },
      { instances },
    );
    const settled = await settleInstanceResolution(resolution, {
      automationId: target.automation.id,
      entrypointId: entrypoint.id,
      eventKey: input.eventKey,
      deliveryKey,
      instances,
    });
    if (settled === "dropped") {
      result.dropped += 1;
      continue;
    }
    const outcome = await admitAutomationRun(
      {
        target,
        runId: automationRunId(target.automation.id, deliveryKey, entrypoint.id, settled.instanceId),
        deliveryKey,
        ...(settled.instanceId !== "" ? { instanceId: settled.instanceId } : {}),
        trigger,
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

/** The one catch-all built-in that rung-1 precedence stands down when a
 * handle-bound workstream owns the event (ADR 0120). Deliberately a single
 * constant, not a registry: suppression is a cross-automation behavior
 * change and each addition should be a reviewed decision. */
const SUPPRESSIBLE_CATCH_ALL = "slack_brain";

/** Built-in keys whose kill switch is ON. Resolved from config by default;
 * tests inject. Each built-in's switch registers its key here — one line per
 * switch, so the dispatcher gate and the route fallback can never disagree.
 * "pr_review" ← ORCHESTRATOR_REVIEW_AUTOMATION_DISABLED (4.4). */
export function disabledBuiltinsFromConfig(): ReadonlySet<string> {
  const keys: string[] = [];
  if (config.reviewAutomationDisabled) keys.push("pr_review");
  // "slack_brain" ← ORCHESTRATOR_SLACK_AUTOMATION_DISABLED (4.6).
  if (config.slackAutomationDisabled) keys.push("slack_brain");
  return new Set(keys);
}

export interface DispatchIntegrationDeps {
  /** Kill-switched built-in keys; their triggers never admit a run. */
  disabledBuiltins?: ReadonlySet<string>;
  store?: IntegrationDispatchStore;
  workflowStarter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  now?: () => Date;
  /** ADR 0120 instances. */
  instances?: AutomationInstanceStore;
  facets?: (provider: string) => Promise<Pick<WebhookFacet, "events"> | undefined>;
}

export interface DispatchIntegrationResult {
  matched: number;
  started: number;
  joined: number;
  queued: number;
  skipped: number;
  /** ADR 0120: instance-admission drops (no run row; audited in the ring). */
  dropped: number;
  /** Built-in keys suppressed by instance precedence (rung 1): a handle-
   * bound workstream owns this event's thread, so the catch-all brain
   * stands down — for the engine AND the legacy route. */
  suppressed: string[];
  /** Targets whose admission threw; the dispatcher rethrows after the loop. */
  failed: number;
  /** Admission outcome per matched BUILT-IN (keyed by builtin key). This is
   * what a legacy route consults to decide whether the engine took the
   * delivery: a built-in absent here (not enabled, kill-switched, scope did
   * not match) or `skipped` leaves the legacy path in charge. The same
   * read the dispatcher made — never a second, possibly stale, lookup. */
  builtins: Record<string, AdmitOutcome>;
}

/** Did the dispatcher hand this delivery to the built-in — a run started,
 * joined, or queued for it? */
export function builtinTookDelivery(
  result: DispatchIntegrationResult | undefined,
  builtinKey: string,
): boolean {
  const outcome = result?.builtins[builtinKey];
  return outcome !== undefined && outcome !== "skipped";
}

/** Instance precedence (ADR 0120 rung 1): was the built-in stood down for
 * this delivery because an open workstream owns the thread? The legacy
 * route must treat this exactly like "the engine took it" — the instance's
 * automation is answering; a second responder is the bug. */
export function builtinSuppressed(
  result: DispatchIntegrationResult | undefined,
  builtinKey: string,
): boolean {
  return result?.suppressed.includes(builtinKey) ?? false;
}

/** Providers whose scope noun compares case-insensitively (GitHub owner/repo).
 * Slack channel ids and Linear team keys are exact. One shared rule with
 * instance handles (automations/handles.ts) so a write and a later match
 * never disagree by case. */
function normalizeScope(provider: string, value: string): string {
  return CASE_INSENSITIVE_HANDLE_PROVIDERS.has(provider) ? value.toLowerCase() : value;
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
  const disabledBuiltins = deps.disabledBuiltins ?? disabledBuiltinsFromConfig();
  const instances = deps.instances ?? defaultInstanceStoreLazy();
  const facets = deps.facets ?? defaultWebhookFacetResolver();

  const targets = (
    await store.listEnabledForIntegrationTrigger(input.provider, input.connectionId)
  ).flatMap((target) => {
    // A built-in's kill switch must stop its TRIGGER path too, not only the
    // legacy route's fallback: otherwise a flagged repo/channel is served by
    // both brains at once (the legacy graph via the route, the built-in via
    // this dispatcher). The switch is the fleet-wide brake; the built-in's
    // own `enabled` toggle is the independent second one.
    if (
      target.automation.builtinKey !== null &&
      disabledBuiltins.has(target.automation.builtinKey)
    ) {
      return [];
    }
    // D9: a delivery matches per ENTRYPOINT — the same event may open (or
    // join) one run for each entrypoint whose trigger matches it.
    return entrypointsOf(target.definition).flatMap((entrypoint) => {
      const trigger = entrypoint.trigger;
      if (trigger.kind !== "integration") return [];
      const matched = matchesIntegrationTrigger(
        trigger,
        {
          provider: input.provider,
          connectionId: input.connectionId,
          eventKey: input.eventKey,
          ...(input.scopeValue !== undefined ? { scopeValue: input.scopeValue } : {}),
        },
        (key) => scopeValuesFromInput(target.automation.inputs, key),
      );
      return matched ? [{ target, entrypoint }] : [];
    });
  });

  const result: DispatchIntegrationResult = {
    matched: targets.length,
    started: 0,
    joined: 0,
    queued: 0,
    skipped: 0,
    dropped: 0,
    suppressed: [],
    failed: 0,
    builtins: {},
  };

  // Only consulted for handle candidates — never load the registry when no
  // matched target is instanced (every pre-instance dispatch, unit tests).
  const anyInstanced = targets.some((t) => t.target.definition.settings.instance !== undefined);
  const facet = anyInstanced ? await facets(input.provider) : undefined;
  const trigger: AutomationRunTrigger = {
    source: "integration",
    eventKey: input.eventKey,
    deliveryId: input.deliveryId,
    payload: input.payload,
    receivedAt: input.receivedAt.toISOString(),
    ...(input.scopeValue !== undefined ? { scopeValue: input.scopeValue } : {}),
  };

  // Instance resolution runs BEFORE admission for every matched pair, so
  // rung-1 precedence can see across targets: when an open workstream owns
  // the event's thread (a handle bound it), the catch-all slack brain
  // stands down for this delivery — one thread, one responder.
  const resolved: Array<{
    target: DispatchTarget;
    entrypoint: { id: string; trigger: TriggerSpec };
    resolution: InstanceResolution;
  }> = [];
  let handleBound = false;
  for (const { target, entrypoint } of targets) {
    const resolution = await resolveInstance(
      { target, entrypoint, trigger, provider: input.provider, ...(facet !== undefined ? { facet } : {}) },
      { instances },
    );
    if (resolution.kind === "bound" && resolution.via === "handle") handleBound = true;
    resolved.push({ target, entrypoint, resolution });
  }

  // Each target is admitted in isolation: one transient fault must never drop
  // the sibling automations matched by the same delivery. Failures are
  // counted and rethrown AFTER every target was tried, so the ingress fails
  // the provider's delivery (it retries; the ledger and the run id dedupe the
  // targets that already succeeded).
  const failures: Array<{ automationId: string; error: unknown }> = [];
  for (const { target, entrypoint, resolution } of resolved) {
    if (
      handleBound &&
      target.automation.builtinKey === SUPPRESSIBLE_CATCH_ALL &&
      !result.suppressed.includes(SUPPRESSIBLE_CATCH_ALL)
    ) {
      result.suppressed.push(SUPPRESSIBLE_CATCH_ALL);
      log.info(
        { provider: input.provider, eventKey: input.eventKey },
        "instance precedence: a workstream owns this thread; the slack brain stands down",
      );
      continue;
    }
    const deliveryKey = `${input.provider}:${input.deliveryId}`;
    try {
      const settled = await settleInstanceResolution(resolution, {
        automationId: target.automation.id,
        entrypointId: entrypoint.id,
        eventKey: input.eventKey,
        deliveryKey,
        instances,
      });
      if (settled === "dropped") {
        result.dropped += 1;
        continue;
      }
      const outcome = await admitAutomationRun(
        {
          target,
          runId: automationRunId(target.automation.id, deliveryKey, entrypoint.id, settled.instanceId),
          entrypoint: { id: entrypoint.id, trigger: entrypoint.trigger },
          ...(settled.instanceId !== "" ? { instanceId: settled.instanceId } : {}),
          deliveryKey,
          trigger,
          scheduledFor: null,
        },
        { store, starter, sender, now },
      );
      result[outcome] += 1;
      if (target.automation.builtinKey !== null) {
        result.builtins[target.automation.builtinKey] = outcome;
      }
    } catch (error) {
      result.failed += 1;
      failures.push({ automationId: target.automation.id, error });
      log.error(
        { automationId: target.automation.id, provider: input.provider, eventKey: input.eventKey, error },
        "integration dispatch: admission failed for one target",
      );
    }
  }

  if (failures.length > 0) {
    throw new IntegrationDispatchError(result, failures);
  }
  return result;
}

/** Raised after every target was tried when at least one admission failed.
 * Carries the partial tally so the caller can fail the delivery (provider
 * retry) without losing what already started. */
export class IntegrationDispatchError extends Error {
  constructor(
    readonly result: DispatchIntegrationResult,
    readonly failures: ReadonlyArray<{ automationId: string; error: unknown }>,
  ) {
    super(
      `integration dispatch: ${failures.length} of ${result.matched} matched automation(s) failed admission`,
    );
    this.name = "IntegrationDispatchError";
  }
}
