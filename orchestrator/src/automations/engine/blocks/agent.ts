/** `run_agent` — the agent step: deliver this run's event to the
 * automation's agent session, or start one (ADR 0119 D9 + contract 5).
 *
 * Not a user block. The definition's `agent` is the config; every way in
 * whose role starts or reaches the agent ends on one step of this type
 * (`entrypointsOf` appends it). What it does:
 *
 *   lookup the workstream's kept session
 *     → found and alive: adopt it
 *     → otherwise: start a session (no opening prompt yet)
 *   install (or re-point) the relay, if the agent talks back somewhere,
 *   and announce a session the relay was just created for
 *   then send the prompt (no wait): the opening prompt to a new session,
 *   the follow-up to a continued one.
 *
 * The relay goes on BEFORE the prompt: a session's first output must find
 * the place it mirrors into, and a continued session's next turn must land
 * under the new mention, not the previous one.
 *
 * Every event is a SHORT run (the house pattern): the durable thing is the
 * kept session plus the open workstream, never a run parked in a loop. The
 * workstream is the agent's scope (`instanceSettingsOf`) — on a run that is
 * not bound to one, `reuse: "workstream"` degrades to a fresh session per
 * event, so one config serves every scope.
 *
 * `close_agent` — the step a `close` way in ends on: post the closing
 * message into the agent's relayed place and remove the relay, end the
 * kept session, then close the workstream (admission drops later events
 * for it).
 */

import { z } from "zod";

import { terminalOutcomeForStatus } from "../../../control-plane/session-events.ts";
import { makeSlackPolicy } from "../../../integrations/slack-policy.ts";
import { sessionWebUrl } from "../../../links.ts";
import { relayFactsOf } from "../../session-relay.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";
import { installRelayForSession, mentionFromRelayConfig, slackRelayConfigSchema } from "./relay.ts";
import { createSessionConfigSchema, executeCreateSession, renderSessionPrompt } from "./session.ts";

export const RUN_AGENT_TYPE = "run_agent";
export const CLOSE_AGENT_TYPE = "close_agent";

export const runAgentConfigSchema = createSessionConfigSchema.extend({
  /** The prompt a NEW session starts with; defaults to `promptTemplate`.
   * A thread's first turn often wants more context (the whole thread so
   * far) than a follow-up (the new message). */
  openingPromptTemplate: z.string().optional(),
  /** `workstream` (default): continue the workstream's kept session when
   * one is alive. `never`: always start a session. */
  reuse: z.enum(["workstream", "never"]).default("workstream"),
  /** Where the agent talks back. Absent = nowhere (the session is reachable
   * on its page only). */
  relay: z.preprocess(
    // A relay whose place is blank (an editor form that left every field
    // empty) is no relay.
    (value) => (isBlankRelay(value) ? undefined : value),
    slackRelayConfigSchema.omit({ session: true }).optional(),
  ),
});

function isBlankRelay(value: unknown): boolean {
  if (value === null || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return ["team", "channel", "threadTs"].every((k) => v[k] === undefined || v[k] === "");
}
export type RunAgentConfig = z.infer<typeof runAgentConfigSchema>;

export const closeAgentConfigSchema = z.object({
  /** Templated close reason, kept on the workstream row for the audit
   * trail and posted into the relayed place. */
  reason: z.string().max(500).optional(),
});
export type CloseAgentConfig = z.infer<typeof closeAgentConfigSchema>;

/** Whether the control plane still holds the session and it is not over.
 * A kept session that ended (completed, failed, swept) is gone: continuing
 * it would wedge the workstream on a session that takes no prompt. */
export async function sessionIsLive(ctx: RunContext, sessionId: string): Promise<boolean> {
  const probe = await ctx.deps.sessions.getSession(sessionId);
  return probe.found && terminalOutcomeForStatus(probe.status) === undefined;
}

/** The workstream's kept session, if it is still live. A session already
 * bound to THIS run counts: a step re-executed after a crash (the adopt or
 * the start happened, the checkpoint did not) converges on it instead of
 * starting a second session for the same workstream. */
async function keptSession(ctx: RunContext): Promise<string | null> {
  if (ctx.instanceId === "") return null;
  const previous = await ctx.deps.store.latestKeptInstanceSession(ctx.instanceId);
  if (previous === null) return null;
  return (await sessionIsLive(ctx, previous.sessionId)) ? previous.sessionId : null;
}

async function execute(config: RunAgentConfig, ctx: RunContext): Promise<BlockOutcome> {
  const { openingPromptTemplate, reuse, relay, ...create } = config;
  const existing = reuse === "workstream" && !ctx.dryRun ? await keptSession(ctx) : null;

  let sessionId: string;
  let started: boolean;
  let text: string;
  let extra: Record<string, unknown> = {};
  if (existing !== null) {
    // Adopt (D11): the binding row moves to this run so the session's idle
    // and end route here while this run lives. A live owner means another
    // run of this workstream is still delivering; the derived `queue`
    // concurrency serializes the runs of one workstream, so this is a
    // retryable error for the rare overlap, never a steal.
    const ref = sessionRefSchema.parse({ template: existing });
    try {
      sessionId = await ctx.resolveSession(ref);
    } catch (error) {
      return {
        kind: "error",
        code: "agent_session_busy",
        message: error instanceof Error ? error.message : String(error),
        retryable: true,
      };
    }
    started = false;
    text = await ctx.render(create.promptTemplate);
  } else {
    // Start the session with no opening prompt: the relay must be on it
    // before its first output, so the prompt goes in after the relay.
    const opening = await executeCreateSession(
      { ...create, promptTemplate: "", includeEventContext: false },
      ctx,
    );
    if (opening.kind !== "ok") return opening;
    sessionId = String(opening.outputs["session_id"]);
    started = true;
    const rendered = await renderSessionPrompt(
      { ...create, promptTemplate: openingPromptTemplate ?? create.promptTemplate },
      ctx,
    );
    if (rendered.kind === "error") return rendered;
    text = rendered.prompt;
    extra = {
      task_id: opening.outputs["task_id"],
      ...(ctx.dryRun ? { dry_run: true, would_execute: opening.outputs["would_execute"] } : {}),
    };
  }

  let relayOutputs: Record<string, unknown> = {};
  if (relay !== undefined) {
    const installed = await installRelayForSession(sessionId, relay, ctx);
    if (installed.kind !== "ok") return installed;
    relayOutputs = { relayed: installed.outputs["installed"] === true, mention_ts: installed.outputs["mention_ts"] };
    // The link goes into the place the moment the relay is created for a
    // session, before any output. A re-executed step finds the relay
    // installed (same mention) and says nothing twice; a continued
    // session's place already carries its link.
    if (installed.outputs["created"] === true) {
      await ctx.deps.announceStarted?.(sessionId, mentionFromRelayConfig(relay));
    }
  }

  // Keyed by frame path and session: the coordinator outbox dedupes on
  // prompt_id, so a step re-executed after a crash never delivers twice.
  const promptId = `autorun:${ctx.runId}:${ctx.currentPath ?? ctx.currentBlockId ?? ""}:${sessionId}`;
  const delivered = text.length > 0;
  if (delivered && !ctx.dryRun) {
    await ctx.deps.sessions.sendPrompt(sessionId, promptId, text, create.harnessMode);
  }

  return {
    kind: "ok",
    outputs: {
      session_id: sessionId,
      web_url: sessionWebUrl(sessionId),
      started,
      delivered,
      prompt: text,
      ...extra,
      ...relayOutputs,
    },
  };
}

async function executeClose(config: CloseAgentConfig, ctx: RunContext): Promise<BlockOutcome> {
  if (ctx.instanceId === "") {
    // Not bound to a workstream (an agent scoped to each event has no
    // standing one): nothing to close.
    return { kind: "ok", outputs: { closed: false, ended: false, not_instanced: true } };
  }
  const reason = config.reason !== undefined ? await ctx.render(config.reason) : undefined;
  if (ctx.dryRun) {
    return {
      kind: "ok",
      outputs: {
        closed: true,
        ended: false,
        not_instanced: false,
        dry_run: true,
        would_execute: { instance_id: ctx.instanceId, ...(reason !== undefined ? { reason } : {}) },
      },
    };
  }
  const ops = ctx.deps.instances;
  if (!ops) {
    return { kind: "error", code: "instance_ops_unavailable", message: "the instance store is not installed", retryable: false };
  }
  const previous = await ctx.deps.store.latestKeptInstanceSession(ctx.instanceId);
  let ended = false;
  if (previous !== null && (await sessionIsLive(ctx, previous.sessionId))) {
    // The relay closes first, with a message that says why, so the place
    // never reads the session's end as an error. Then the session ends; the
    // consumer finds no relay for it and posts nothing more.
    const relays = ctx.deps.relays;
    const row = relays ? await relays.get(previous.sessionId) : null;
    if (relays && row !== null) {
      const facts = relayFactsOf(row, { sessionWebUrl });
      const policy = makeSlackPolicy({ routeExtras: { sessionId: row.sessionId } });
      await policy.onNeutralClose(facts.mention, reason ?? "This agent was closed.");
      await relays.remove(row.sessionId);
    }
    await ctx.deps.sessions.endSession(previous.sessionId);
    ended = true;
  }
  // false = already closed (an earlier attempt or a manual close):
  // idempotent success, the run's intent holds either way.
  const closed = await ops.closeInstance({
    instanceId: ctx.instanceId,
    ...(reason !== undefined ? { reason } : {}),
  });
  return {
    kind: "ok",
    outputs: {
      closed,
      ended,
      not_instanced: false,
      ...(previous !== null ? { session_id: previous.sessionId } : {}),
    },
  };
}

export function registerRunAgentBlock(): void {
  registerBlock<RunAgentConfig>({
    type: RUN_AGENT_TYPE,
    outputs: ["session_id", "web_url", "started", "delivered", "prompt", "task_id", "relayed", "mention_ts"],
    configSchema: runAgentConfigSchema,
    execute,
  });
  registerBlock<CloseAgentConfig>({
    type: CLOSE_AGENT_TYPE,
    outputs: ["closed", "ended", "session_id", "not_instanced"],
    configSchema: closeAgentConfigSchema,
    execute: executeClose,
  });
}
