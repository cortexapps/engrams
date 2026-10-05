/** Verified webhook occurrence routing into the interpreter workflow
 * (ADR 0102 ingress → ADR 0119 admission).
 *
 * Admission (concurrency policy) is decided here, before any workflow starts:
 * queue leaves a pending run for the holder's finalize to promote; supersede
 * CASes the claim and signals the old run; skip records an auditable filtered
 * run; join routes the delivery into the active run's mailbox.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { log as rootLog } from "../log.ts";
import { makeCodeBlockRuntime } from "./code/runtime.ts";
import { resolveAutomationInputs } from "../db/automations.ts";
import { evaluateAdmissionPrelude, type AdmissionVerdict } from "./engine/admission.ts";
import type { CodeBlockRuntime } from "./engine/deps.ts";

import {
  makeAutomationStore,
  type AutomationDispatchStore,
  type DispatchTarget,
  type WebhookRegistrationRow,
} from "../db/automations.ts";
import type { AutomationRunTrigger, AutomationTrigger } from "../db/schema.ts";
import type { IntegrationEventDispatchInput } from "./integration-ingress.ts";
import { CASE_INSENSITIVE_HANDLE_PROVIDERS, extractHandleCandidates } from "./handles.ts";
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
  /** The admission prelude rejected the delivery (a filtered run row, no
   * claim, no workflow). */
  filtered: number;
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
  /** The workstream's input snapshot (layered over the automation's inputs
   * the way loadSnapshot does), so the admission prelude reads the inputs
   * the run will see. */
  instanceInputs?: Record<string, unknown>;
  /** The admission-prelude verdict dispatch already took for this delivery
   * (before opening its workstream). Absent = evaluate it here. */
  admission?: AdmissionVerdict;
  trigger: AutomationRunTrigger;
  scheduledFor: Date | null;
  /** Editor DryRun: the run row is flagged and integration actions stub. */
  dryRun?: boolean;
}

/** `filtered` = the admission prelude rejected the delivery before any
 * claim: a run row records it, no workflow starts. */
export type AdmitOutcome = "started" | "joined" | "queued" | "skipped" | "filtered";

let sharedCodeRuntime: CodeBlockRuntime | undefined;
function codeRuntime(): CodeBlockRuntime {
  return (sharedCodeRuntime ??= makeCodeBlockRuntime());
}

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
): Promise<Exclude<AdmitOutcome, "filtered">> {
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

/** Evaluate the entrypoint's admission prelude against the inputs the run
 * would see (the workstream snapshot layered over the automation's). */
async function preludeVerdict(args: {
  target: DispatchTarget;
  entrypoint: { id: string; trigger: TriggerSpec };
  trigger: AutomationRunTrigger;
  deliveryKey?: string;
  instanceInputs?: Record<string, unknown> | undefined;
  scheduledFor: Date | null;
  now: () => Date;
  code?: CodeBlockRuntime | undefined;
}): Promise<AdmissionVerdict> {
  const { target, trigger } = args;
  const receivedAt = trigger.receivedAt ?? args.now().toISOString();
  return evaluateAdmissionPrelude({
    definition: target.definition,
    inputs: resolveAutomationInputs(target.definition.inputsSchema, {
      ...target.automation.inputs,
      ...(args.instanceInputs ?? {}),
    }),
    automationId: target.automation.id,
    automationName: target.automation.name,
    trigger: {
      kind: trigger.source,
      receivedAt,
      ...(trigger.eventKey !== undefined ? { eventKey: trigger.eventKey } : {}),
      ...(args.deliveryKey !== undefined ? { deliveryKey: args.deliveryKey } : {}),
      ...(trigger.payload !== undefined ? { payload: trigger.payload } : {}),
      ...(args.scheduledFor ? { scheduledFor: args.scheduledFor.toISOString() } : {}),
    },
    aliases: [],
    entrypointId: args.entrypoint.id,
    code: args.code ?? codeRuntime(),
  });
}

/** A prelude rejection leaves a `filtered` run row (no workflow, no claim)
 * so the Activity tab shows the delivery arrived and why nothing happened. */
async function recordFilteredAdmission(args: {
  store: AutomationDispatchStore;
  target: DispatchTarget;
  entrypointId: string;
  instanceId: string;
  runId: string;
  trigger: AutomationRunTrigger;
  deliveryKey: string;
  scheduledFor: Date | null;
  reason: string;
  now: () => Date;
  dryRun?: boolean | undefined;
}): Promise<void> {
  await args.store.insertRun({
    id: args.runId,
    automationId: args.target.automation.id,
    version: args.target.automation.currentVersion,
    entrypointId: args.entrypointId,
    ...(args.instanceId !== "" ? { instanceId: args.instanceId } : {}),
    trigger: args.trigger,
    deliveryKey: args.deliveryKey,
    concurrencyKey: null,
    scheduledFor: args.scheduledFor,
    status: "filtered",
    error: args.reason,
    endedAt: args.now(),
    ...(args.dryRun ? { dryRun: true } : {}),
  });
}

/** Decide one matched (target, entrypoint) for a delivery, in the order that
 * keeps side effects honest: an instance DROP first (no run, audited); then
 * the admission prelude against the workstream's inputs — BEFORE the
 * workstream is opened, so a filtered delivery ("LGTM" on a PR nobody asked
 * to review, a draft PR opening) never creates a workstream; then the
 * settle (open or join) and the concurrency claim. */
async function decideAdmission(args: {
  target: DispatchTarget;
  entrypoint: { id: string; trigger: TriggerSpec };
  trigger: AutomationRunTrigger;
  resolution: InstanceResolution;
  deliveryKey: string;
  eventKey: string;
  instances: AutomationInstanceStore;
  store: AutomationDispatchStore;
  now: () => Date;
  code?: CodeBlockRuntime | undefined;
}): Promise<
  | { kind: "dropped" }
  | { kind: "filtered" }
  | { kind: "admit"; instanceId: string; inputs?: Record<string, unknown>; verdict: AdmissionVerdict }
> {
  const { target, entrypoint, trigger, resolution, deliveryKey } = args;
  const settleCtx = {
    automationId: target.automation.id,
    entrypointId: entrypoint.id,
    eventKey: args.eventKey,
    deliveryKey,
    instances: args.instances,
  };
  if (resolution.kind === "drop") {
    await settleInstanceResolution(resolution, settleCtx);
    return { kind: "dropped" };
  }
  const boundId = resolution.kind === "bound" ? resolution.instance.id : "";
  const instanceInputs =
    resolution.kind === "bound"
      ? resolution.instance.inputs
      : resolution.kind === "open"
        ? resolution.inputs
        : undefined;
  const verdict = await preludeVerdict({
    target,
    entrypoint,
    trigger,
    deliveryKey,
    instanceInputs,
    scheduledFor: null,
    now: args.now,
    code: args.code,
  });
  if (verdict.kind === "reject") {
    await recordFilteredAdmission({
      store: args.store,
      target,
      entrypointId: entrypoint.id,
      instanceId: boundId,
      runId: automationRunId(target.automation.id, deliveryKey, entrypoint.id, boundId),
      trigger,
      deliveryKey,
      scheduledFor: null,
      reason: verdict.reason,
      now: args.now,
    });
    return { kind: "filtered" };
  }
  const settled = await settleInstanceResolution(resolution, settleCtx);
  if (settled === "dropped") return { kind: "dropped" };
  return { kind: "admit", instanceId: settled.instanceId, inputs: settled.inputs, verdict };
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
    /** Evaluates the admission prelude's code blocks; the shared QuickJS
     * runtime by default. */
    code?: CodeBlockRuntime;
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

  // The graph's own admission decides BEFORE the concurrency claim (see
  // engine/admission.ts): a delivery the prelude filters never supersedes
  // or joins a live run. Dispatch passes the verdict it already took
  // (before opening any workstream); a direct caller gets it evaluated here.
  {
    const verdict =
      input.admission ??
      (await preludeVerdict({
        target,
        entrypoint,
        trigger,
        instanceInputs: input.instanceInputs,
        scheduledFor: input.scheduledFor,
        now: deps.now,
        code: deps.code,
      }));
    if (verdict.kind === "reject") {
      await recordFilteredAdmission({
        store: deps.store,
        target,
        entrypointId: entrypoint.id,
        instanceId,
        runId,
        trigger,
        deliveryKey,
        scheduledFor: input.scheduledFor,
        reason: verdict.reason,
        now: deps.now,
        dryRun: input.dryRun,
      });
      return "filtered";
    }
  }

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
    openHandleOwners: (handles) => get().openHandleOwners(handles),
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
): Promise<{ instanceId: string; inputs?: Record<string, unknown> } | "dropped"> {
  switch (resolution.kind) {
    case "none":
      return { instanceId: "" };
    case "bound":
      return { instanceId: resolution.instance.id, inputs: resolution.instance.inputs };
    case "open": {
      const instance = await ctx.instances.openInstance({
        automationId: ctx.automationId,
        key: resolution.key,
        label: resolution.label,
        inputs: resolution.inputs,
        openedBy: `event:${ctx.deliveryKey}`,
      });
      return { instanceId: instance.id, inputs: instance.inputs };
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
    filtered: 0,
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
    const decided = await decideAdmission({
      target,
      entrypoint,
      trigger,
      resolution,
      deliveryKey,
      eventKey: input.eventKey,
      instances,
      store,
      now,
    });
    if (decided.kind === "dropped") {
      result.dropped += 1;
      continue;
    }
    if (decided.kind === "filtered") {
      result.filtered += 1;
      continue;
    }
    const outcome = await admitAutomationRun(
      {
        target,
        runId: automationRunId(target.automation.id, deliveryKey, entrypoint.id, decided.instanceId),
        deliveryKey,
        ...(decided.instanceId !== "" ? { instanceId: decided.instanceId, instanceInputs: decided.inputs } : {}),
        admission: decided.verdict,
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

/** The one catch-all built-in that rung-1 precedence stands down when
 * ANOTHER automation's handle-bound workstream owns the event's conversation
 * (ADR 0120). Deliberately a single constant, not a registry: suppression is
 * a cross-automation behavior change and each addition should be a reviewed
 * decision. */
const SUPPRESSIBLE_CATCH_ALL = "slack_brain";

export interface DispatchIntegrationDeps {
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
  /** The admission prelude rejected the delivery (a filtered run row, no
   * claim, no workflow). */
  filtered: number;
  /** ADR 0120: instance-admission drops (no run row; audited in the ring). */
  dropped: number;
  /** Built-in keys suppressed by instance precedence (rung 1): another
   * automation's handle-bound workstream owns this event's conversation, so
   * the catch-all brain stood down for this delivery. */
  suppressed: string[];
  /** Targets whose admission threw; the dispatcher rethrows after the loop. */
  failed: number;
  /** Admission outcome per matched BUILT-IN (keyed by builtin key): the
   * delivery's verdict, as the dispatcher made it — a built-in absent here
   * was not matched (not enabled, scope did not match). */
  builtins: Record<string, AdmitOutcome>;
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
  const instances = deps.instances ?? defaultInstanceStoreLazy();
  const facets = deps.facets ?? defaultWebhookFacetResolver();

  const providerTargets = await store.listEnabledForIntegrationTrigger(
    input.provider,
    input.connectionId,
  );
  // Conversation-ownership gate (rung 2): computed over the PROVIDER's
  // enabled automations BEFORE event-key filtering — a channel owner
  // subscribed only to `message` must still suppress the brain for the
  // `app_mention` twin, where it is not an event-matched target. When no
  // instanced automation exists for the provider at all, the ledger is
  // never consulted (the no-instances world stays DB-free).
  const anyInstancedForProvider = providerTargets.some(
    (target) => target.definition.settings.instance !== undefined,
  );
  const targets = providerTargets.flatMap((target) => {
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
    filtered: 0,
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
  // rung-1 precedence can see across targets: when an open workstream of
  // ANOTHER automation owns the event's thread (a handle bound it), the
  // catch-all slack brain stands down for this delivery — one thread, one
  // responder. The brain's OWN workstream owning the thread is the normal
  // case for a live thread (its "Started a session" post binds the thread
  // handle to it) and routes the event to that workstream by key; it must
  // never stand the brain down (prod 2026-10-01: every follow-up in every
  // brain thread was suppressed by the brain's own binding). The verdict is
  // computed over the PROVIDER's instanced automations even when no brain is
  // a matched target, so `suppressed` is always this delivery's truth.
  const resolved: Array<{
    target: DispatchTarget;
    entrypoint: { id: string; trigger: TriggerSpec };
    resolution: InstanceResolution;
  }> = [];
  const owners = new Map<string, string>(); // instanceId → automationId
  for (const { target, entrypoint } of targets) {
    const resolution = await resolveInstance(
      { target, entrypoint, trigger, provider: input.provider, ...(facet !== undefined ? { facet } : {}) },
      { instances },
    );
    if (resolution.kind === "bound" && resolution.via === "handle") {
      owners.set(resolution.instance.id, resolution.instance.automationId);
    }
    resolved.push({ target, entrypoint, resolution });
  }
  // Rung 2 completion: ownership is about the CONVERSATION, not the event
  // subscription. A tagged message arrives as TWO deliveries (message +
  // app_mention), and the owning workstream may subscribe to only one of
  // them — but every brain must stand down for both (prod 2026-08-26: the
  // legacy picker answered a mention in an owned channel because no MATCHED
  // target was instanced, so no handle ever resolved). Ask the ledger
  // which open workstreams — in any automation — own one of the event's
  // candidate handles. Deliberately NOT gated on the suppressible built-in
  // being a matched target: the verdict is this delivery's, whoever reads
  // it (prod 2026-08-26, second finding: the built-in was disabled, the
  // gate skipped the check, and a consumer of the verdict spawned a session
  // in an owned channel with the fix fully deployed).
  if (anyInstancedForProvider) {
    const suppressFacet = facet ?? (await facets(input.provider));
    if (suppressFacet !== undefined) {
      const candidates = extractHandleCandidates({
        provider: input.provider,
        facet: suppressFacet,
        eventKey: input.eventKey,
        payload: input.payload ?? {},
      });
      if (candidates.length > 0) {
        for (const owner of await instances.openHandleOwners(candidates)) {
          owners.set(owner.instanceId, owner.automationId);
        }
      }
    }
  }
  // The suppression VERDICT is a property of the delivery, not of which
  // brains happen to be enabled: record it whenever ownership held, so the
  // legacy route stands down even when the built-in is not a target (the
  // owner may be the built-in brain's own thread — the legacy brain still
  // has no business in it). The per-target loop below skips the matched
  // built-in only when ANOTHER automation's workstream owns the thread.
  const catchAllIds = new Set(
    targets
      .filter(({ target }) => target.automation.builtinKey === SUPPRESSIBLE_CATCH_ALL)
      .map(({ target }) => target.automation.id),
  );
  const ownedByOther = [...owners.values()].some((automationId) => !catchAllIds.has(automationId));
  if (ownedByOther && !result.suppressed.includes(SUPPRESSIBLE_CATCH_ALL)) {
    result.suppressed.push(SUPPRESSIBLE_CATCH_ALL);
    log.info(
      { provider: input.provider, eventKey: input.eventKey },
      "instance precedence: another automation's workstream owns this conversation; the brain stands down",
    );
  }

  // Each target is admitted in isolation: one transient fault must never drop
  // the sibling automations matched by the same delivery. Failures are
  // counted and rethrown AFTER every target was tried, so the ingress fails
  // the provider's delivery (it retries; the ledger and the run id dedupe the
  // targets that already succeeded).
  const failures: Array<{ automationId: string; error: unknown }> = [];
  for (const { target, entrypoint, resolution } of resolved) {
    if (ownedByOther && target.automation.builtinKey === SUPPRESSIBLE_CATCH_ALL) {
      // The verdict (result.suppressed) and its log were recorded above,
      // once per delivery; here the matched built-in is only skipped.
      continue;
    }
    const deliveryKey = `${input.provider}:${input.deliveryId}`;
    try {
      const decided = await decideAdmission({
        target,
        entrypoint,
        trigger,
        resolution,
        deliveryKey,
        eventKey: input.eventKey,
        instances,
        store,
        now,
      });
      if (decided.kind === "dropped") {
        result.dropped += 1;
        continue;
      }
      const outcome: AdmitOutcome =
        decided.kind === "filtered"
          ? "filtered"
          : await admitAutomationRun(
              {
                target,
                runId: automationRunId(target.automation.id, deliveryKey, entrypoint.id, decided.instanceId),
                entrypoint: { id: entrypoint.id, trigger: entrypoint.trigger },
                ...(decided.instanceId !== ""
                  ? { instanceId: decided.instanceId, instanceInputs: decided.inputs }
                  : {}),
                admission: decided.verdict,
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
