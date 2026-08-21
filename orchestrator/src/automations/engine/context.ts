/** The run context blocks read and write (ADR 0119 D1).
 *
 * One JSON scope: inputs.*, trigger.*, event.* (curated aliases + redacted
 * raw), steps.<blockId>.*, and run metadata. Liquid renders prose through the
 * hardened ADR 0102 engine; nothing here widens the template sandbox.
 */

import {
  buildAutomationTemplateContext,
  renderAutomationTemplateInScope,
  type AutomationTemplateContext,
  type WebhookAliasMapping,
} from "../template.ts";
import type { AutomationDefinition, AutomationSettings, SessionRef } from "./definition.ts";
import type { EngineDeps } from "./deps.ts";
import { ownPath } from "../paths.ts";

export interface RunTriggerFacts {
  kind: "cron" | "webhook" | "integration" | "manual";
  receivedAt: string;
  eventKey?: string;
  scheduledFor?: string;
  deliveryKey?: string;
  /** Redacted provider payload. */
  payload?: Record<string, unknown>;
}

/** The checkpointed snapshot the interpreter walks (output of
 * step:__snapshot__:0). Replay sees exactly this object. */
export interface RunSnapshot {
  definition: AutomationDefinition;
  inputs: Record<string, unknown>;
  automationId: string;
  automationName: string;
  version: number;
  trigger: RunTriggerFacts;
  aliases: WebhookAliasMapping[];
  concurrencyKey?: string;
  startedAtMs: number;
  /** Editor dry run: integration actions are stubbed (record, never call). */
  dryRun?: boolean;
}

export interface RunContext {
  runId: string;
  automationId: string;
  automationName: string;
  version: number;
  settings: AutomationSettings;
  inputs: Record<string, unknown>;
  trigger: AutomationTemplateContext["trigger"] & Record<string, unknown>;
  event: AutomationTemplateContext["event"];
  /** Block outputs. Keyed by plain block id (loop iterations last-write-win)
   * AND by full frame path for disambiguation. */
  steps: Record<string, Record<string, unknown>>;
  loop?: { index: number };
  /** Set by the interpreter for the duration of one block's execute/wait. */
  currentBlockId?: string;
  currentAttempt?: number;
  /** True for an editor DryRun (see RunSnapshot.dryRun). */
  dryRun: boolean;
  deps: EngineDeps;
  render(template: string): Promise<string>;
  resolveSession(ref: SessionRef): Promise<string>;
  /** The flat object condition paths resolve against. */
  scope(): Record<string, unknown>;
}

export function buildRunContext(
  runId: string,
  snapshot: RunSnapshot,
  deps: EngineDeps,
): RunContext {
  const legacyKind = snapshot.trigger.kind === "cron" ? "cron" : "webhook";
  const base = buildAutomationTemplateContext({
    automationName: snapshot.automationName,
    triggerKind: legacyKind,
    receivedAt: snapshot.trigger.receivedAt,
    ...(snapshot.trigger.eventKey ? { eventKey: snapshot.trigger.eventKey } : {}),
    ...(snapshot.trigger.scheduledFor ? { scheduledFor: snapshot.trigger.scheduledFor } : {}),
    ...(snapshot.trigger.payload ? { rawPayload: snapshot.trigger.payload } : {}),
    aliases: snapshot.aliases,
  });
  // `trigger.kind` carries the real kind; the legacy cron|webhook narrowing
  // exists only inside buildAutomationTemplateContext's input type.
  const trigger = Object.assign(Object.create(null) as Record<string, unknown>, base.trigger, {
    kind: snapshot.trigger.kind,
    ...(snapshot.trigger.deliveryKey ? { delivery_key: snapshot.trigger.deliveryKey } : {}),
  }) as RunContext["trigger"];

  const steps: Record<string, Record<string, unknown>> = Object.create(null);

  const ctx: RunContext = {
    runId,
    automationId: snapshot.automationId,
    automationName: snapshot.automationName,
    version: snapshot.version,
    settings: snapshot.definition.settings,
    dryRun: snapshot.dryRun === true,
    inputs: snapshot.inputs,
    trigger,
    event: base.event,
    steps,
    deps,
    scope() {
      return {
        inputs: ctx.inputs,
        trigger: ctx.trigger,
        event: ctx.event,
        steps: ctx.steps,
        run: { id: runId, automation: { id: ctx.automationId, name: ctx.automationName } },
      };
    },
    async render(template: string): Promise<string> {
      return renderAutomationTemplateInScope(template, ctx.scope());
    },
    async resolveSession(ref: SessionRef): Promise<string> {
      if ("blockId" in ref) {
        const sessionId = ownPath(ctx.steps, `${ref.blockId}.session_id`);
        if (typeof sessionId !== "string" || sessionId === "") {
          throw new Error(`block "${ref.blockId}" has no session_id output`);
        }
        return sessionId;
      }
      const rendered = await ctx.render(ref.template);
      if (rendered === "") throw new Error("session reference rendered empty");
      return rendered;
    },
  };
  return ctx;
}

/** Record a finished block's outputs under both addresses. */
export function recordStepOutputs(
  ctx: RunContext,
  blockId: string,
  framePath: string,
  outputs: Record<string, unknown>,
): void {
  ctx.steps[blockId] = outputs;
  if (framePath !== blockId) ctx.steps[framePath] = outputs;
}
