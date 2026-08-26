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
  /** ADR 0119 D9: which entrypoint's blocks this run walks. Absent (an
   * old checkpointed snapshot) = the main entrypoint. */
  entrypointId?: string;
  /** ADR 0120: the bound workstream. Absent (old snapshot / unbound run) =
   * automation-scoped behavior — additive, so no contract bump (the D9
   * precedent). */
  instanceId?: string;
  instanceKey?: string;
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
  /** ADR 0120: the bound workstream ('' = unbound). */
  instanceId: string;
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
  /** The block's frame path (`loop[2].post`, `__finalize__.recap`): the
   * run-durable identity of THIS execution. Every idempotency key a block
   * mints (prompt ids, action client ids, markers) derives from it, never
   * from `currentBlockId` — a block inside a loop runs once per iteration
   * and each must reach its own external resource. */
  currentPath?: string;
  currentAttempt?: number;
  /** The installed handler's state as of its previous invocation (contract
   * 3, see BlockExecutor.onMessage): a checkpointed step output, so it
   * survives recovery. Seeded from the install step's `outputs.handler_state`. */
  handlerState?: Record<string, unknown>;
  /** Set once the run has a terminal status, before finalize hooks run, so
   * a hook's templates can read `run.status` / `run.error`. */
  terminal?: { status: string; error?: string };
  /** True for an editor DryRun (see RunSnapshot.dryRun). */
  dryRun: boolean;
  deps: EngineDeps;
  render(template: string): Promise<string>;
  /** Resolve a session ref to a session id. A `{blockId}` ref reads the
   * block's recorded `session_id`. A `{template}` ref renders the id and —
   * outside dry runs — ADOPTS it (D11): the session must be bound to THIS
   * automation (the binding row is the ownership boundary), and the row
   * re-binds to this run so waits and relays route here. Re-binding only
   * succeeds when the owning run is terminal — adoption never steals
   * routing from a live run. The read-only `session_status` block does its
   * own binding read instead, where "unbound" is a value, not an error. */
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
    instanceId: snapshot.instanceId ?? "",
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
        run: {
          id: runId,
          automation: { id: ctx.automationId, name: ctx.automationName },
          ...(ctx.terminal
            ? { status: ctx.terminal.status, error: ctx.terminal.error ?? "" }
            : {}),
        },
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
      // Dry runs skip the binding checks: no dry block ever acts on the id,
      // and a dry-run:<...> fake could never pass them.
      if (ctx.dryRun) return rendered;
      const outcome = await deps.store.adoptSession({
        runId,
        automationId: ctx.automationId,
        sessionId: rendered,
        instanceId: snapshot.instanceId ?? "",
      });
      if (outcome === "foreign") {
        throw new Error(
          `session "${rendered}" is not bound to this automation (the binding row is the ownership boundary)`,
        );
      }
      if (outcome === "owner_live") {
        throw new Error(
          `session "${rendered}" belongs to a live run of this automation; adoption never steals routing (probe with session_status, or serialize through the entity's concurrency key)`,
        );
      }
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
