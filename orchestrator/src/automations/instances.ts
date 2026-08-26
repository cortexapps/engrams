/** Instance admission (ADR 0120 addendum): resolve which workstream a
 * matched (target, entrypoint) occurrence belongs to, BEFORE the run id is
 * minted. A leaf over the instance store + template renderer; both
 * dispatchers and the ad-hoc RPC path call through here.
 *
 * Route order: handles first (an event that names a thread/PR the ledger
 * knows belongs to that instance, whatever the key template says), then the
 * key template. Dropping happens here — a dropped event mints NO run row
 * (webhook-scale noise costs nothing) and is recorded in the drops ring so
 * "why didn't it fire" stays answerable.
 */

import type { WebhookFacet } from "../connectors/registry.ts";
import type {
  AutomationInstanceRow,
  AutomationInstanceStore,
} from "../db/automation-instances.ts";
import type { AutomationRunTrigger, InputFieldSpec } from "../db/schema.ts";
import type { DispatchTarget } from "../db/automations.ts";
import { resolveAutomationInputs } from "../db/automations.ts";
import type { InstanceAdmitPolicy, TriggerSpec } from "./engine/definition.ts";
import { extractHandleCandidates } from "./handles.ts";
import { renderAutomationTemplateInScope } from "./template.ts";

export type InstanceDropReason = "closed_instance" | "no_handle_match" | "no_open_instance";

export type InstanceResolution =
  /** Not an instanced automation: everything stays byte-identical. */
  | { kind: "none" }
  /** The event belongs to this existing instance. */
  | { kind: "bound"; instance: AutomationInstanceRow; via: "handle" | "key" }
  /** Admission should open (or join, racing) this instance. */
  | { kind: "open"; key: string; inputs: Record<string, unknown> }
  /** Drop before any run row exists. */
  | { kind: "drop"; reason: InstanceDropReason; detail: string };

/** The one template scope admission renders in — shared byte-for-byte with
 * the concurrency key (authors learn ONE contract). */
export function admissionScope(
  target: DispatchTarget,
  trigger: AutomationRunTrigger,
): Record<string, unknown> {
  return {
    trigger: {
      kind: trigger.source,
      ...(trigger.eventKey !== undefined ? { event: trigger.eventKey } : {}),
      ...(trigger.receivedAt !== undefined ? { received_at: trigger.receivedAt } : {}),
    },
    event: { raw: trigger.payload ?? {} },
    inputs: target.automation.inputs,
  };
}

/** Per-instance concurrency scoping: prefix AFTER the template renders, so
 * claims/CAS/promotion isolate per instance with no key-table migration.
 * Instance ids contain no ":", so the mapping is injective. */
export function instanceConcurrencyKey(instanceId: string, key: string): string {
  return instanceId === "" ? key : `i:${instanceId}:${key}`;
}

/** Instance-input snapshot: automation-row values overlaid with the
 * entrypoint's rendered templates, coerced to the field's declared type
 * (templates render strings; a number/boolean field would otherwise fail
 * the snapshot-time validation forever). */
async function renderInstanceInputs(
  templates: Record<string, string>,
  inputsSchema: InputFieldSpec[],
  scope: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const rendered: Record<string, unknown> = {};
  for (const [key, template] of Object.entries(templates)) {
    const value = await renderAutomationTemplateInScope(template, scope);
    const field = inputsSchema.find((f) => f.key === key);
    if (field?.type === "number") {
      const parsed = Number(value);
      rendered[key] = Number.isFinite(parsed) ? parsed : value;
    } else if (field?.type === "boolean") {
      rendered[key] = value === "true" ? true : value === "false" ? false : value;
    } else {
      rendered[key] = value;
    }
  }
  return rendered;
}

export interface ResolveInstanceInput {
  target: DispatchTarget;
  entrypoint: { id: string; trigger: TriggerSpec };
  trigger: AutomationRunTrigger;
  /** The delivering provider (handle canonicalization + facet events);
   * absent for manual/cron occurrences (no candidate handles). */
  provider?: string;
  facet?: Pick<WebhookFacet, "events">;
}

export interface ResolveInstanceDeps {
  instances: Pick<AutomationInstanceStore, "resolveHandles" | "getOpenInstanceByKey" | "getInstance">;
}

export async function resolveInstance(
  input: ResolveInstanceInput,
  deps: ResolveInstanceDeps,
): Promise<InstanceResolution> {
  const settings = input.target.definition.settings.instance;
  if (!settings) return { kind: "none" };
  const automationId = input.target.automation.id;
  const admit: InstanceAdmitPolicy =
    settings.entrypoints?.[input.entrypoint.id]?.admit ?? "open";

  // Tier 1 — the handle ledger. An event that names something an instance
  // owns belongs to that instance, whatever the key template would say.
  const candidates =
    input.provider !== undefined && input.trigger.eventKey !== undefined
      ? extractHandleCandidates({
          provider: input.provider,
          facet: input.facet,
          eventKey: input.trigger.eventKey,
          payload: input.trigger.payload ?? {},
        })
      : [];
  if (candidates.length > 0) {
    const hits = await deps.instances.resolveHandles(automationId, candidates);
    const hit = candidates
      .map((handle) => hits.find((h) => h.handle === handle))
      .find((h) => h !== undefined);
    if (hit) {
      if (hit.instanceStatus === "closed") {
        // v1 policy (documented in the ADR addendum): a closed workstream's
        // threads stay closed — drop, audited. Successor instances are the
        // named future option.
        return { kind: "drop", reason: "closed_instance", detail: hit.handle };
      }
      const instance = await deps.instances.getInstance(hit.instanceId);
      if (instance) return { kind: "bound", instance, via: "handle" };
    }
  }
  if (admit === "handle_match") {
    return {
      kind: "drop",
      reason: "no_handle_match",
      detail: candidates.join(",") || `event:${input.trigger.eventKey ?? input.trigger.source}`,
    };
  }

  // Tier 2 — the identity key template.
  const scope = admissionScope(input.target, input.trigger);
  const key = await renderAutomationTemplateInScope(settings.keyTemplate, scope);
  const existing = await deps.instances.getOpenInstanceByKey(automationId, key);
  if (existing) return { kind: "bound", instance: existing, via: "key" };
  if (admit === "require") {
    return { kind: "drop", reason: "no_open_instance", detail: key };
  }
  const inputs = await renderInstanceInputs(
    settings.inputs ?? {},
    input.target.definition.inputsSchema,
    scope,
  );
  const defaults = resolveAutomationInputs(
    input.target.definition.inputsSchema,
    input.target.automation.inputs,
  );
  return { kind: "open", key, inputs: { ...defaults, ...inputs } };
}
