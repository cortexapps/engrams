/** `run_agent` — the agent step: deliver this run's event to the
 * automation's agent session, or start one (ADR 0119 D9 + contract 5).
 *
 * Not a user block. The definition's `agent` is the config; every way in
 * whose role starts or reaches the agent ends on one step of this type
 * (`entrypointsOf` appends it). What it does:
 *
 *   lookup the workstream's kept session
 *     → found and alive: adopt it, send the prompt (no wait)
 *     → otherwise: create a session with the opening prompt
 *   then install (or re-point) the relay, if the agent talks back somewhere,
 *   and announce a new session there.
 *
 * Every event is a SHORT run (the house pattern): the durable thing is the
 * kept session plus the open workstream, never a run parked in a loop. The
 * workstream is the agent's scope (`instanceSettingsOf`) — on a run that is
 * not bound to one, `reuse: "workstream"` degrades to a fresh session per
 * event, so one config serves every scope.
 */

import { z } from "zod";

import { sessionWebUrl } from "../../../links.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";
import { installRelayForSession, mentionFromRelayConfig, slackRelayConfigSchema } from "./relay.ts";
import { createSessionConfigSchema, executeCreateSession } from "./session.ts";

export const RUN_AGENT_TYPE = "run_agent";

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

/** The workstream's kept session, if it still exists. */
async function keptSession(ctx: RunContext): Promise<string | null> {
  if (ctx.instanceId === "") return null;
  const previous = await ctx.deps.store.latestKeptInstanceSession(ctx.instanceId, ctx.runId);
  if (previous === null) return null;
  const probe = await ctx.deps.sessions.getSession(previous.sessionId);
  return probe.found ? previous.sessionId : null;
}

async function execute(config: RunAgentConfig, ctx: RunContext): Promise<BlockOutcome> {
  const { openingPromptTemplate, reuse, relay, ...create } = config;
  const existing = reuse === "workstream" && !ctx.dryRun ? await keptSession(ctx) : null;

  let sessionId: string;
  let started: boolean;
  let delivered: boolean;
  let extra: Record<string, unknown> = {};
  if (existing !== null) {
    // Adopt (D11): the binding row moves to this run so the session's idle
    // and end route here while this run lives. A live owner means another
    // run of this workstream is still delivering; that is the concurrency
    // claim's job to serialize, so it is a retryable error, never a steal.
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
    const text = await ctx.render(create.promptTemplate);
    const promptId = `autorun:${ctx.runId}:${ctx.currentPath ?? ctx.currentBlockId ?? ""}:${sessionId}`;
    if (text.length > 0) {
      await ctx.deps.sessions.sendPrompt(sessionId, promptId, text, create.harnessMode);
    }
    started = false;
    delivered = text.length > 0;
    extra = { prompt: text };
  } else {
    const opening = await executeCreateSession(
      { ...create, promptTemplate: openingPromptTemplate ?? create.promptTemplate },
      ctx,
    );
    if (opening.kind !== "ok") return opening;
    sessionId = String(opening.outputs["session_id"]);
    started = true;
    delivered = opening.outputs["initial_prompt"] === true;
    extra = { task_id: opening.outputs["task_id"], prompt: opening.outputs["prompt"], ...(ctx.dryRun ? { dry_run: true, would_execute: opening.outputs["would_execute"] } : {}) };
  }

  let relayOutputs: Record<string, unknown> = {};
  if (relay !== undefined) {
    const installed = await installRelayForSession(sessionId, relay, ctx);
    if (installed.kind !== "ok") return installed;
    relayOutputs = { relayed: installed.outputs["installed"] === true, mention_ts: installed.outputs["mention_ts"] };
    // A new session's link goes into the place as soon as it exists, before
    // any output; a continued session's place already carries it.
    if (started && installed.outputs["installed"] === true) {
      await ctx.deps.announceStarted?.(sessionId, mentionFromRelayConfig(relay));
    }
  }

  return {
    kind: "ok",
    outputs: {
      session_id: sessionId,
      web_url: sessionWebUrl(sessionId),
      started,
      delivered,
      ...extra,
      ...relayOutputs,
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
}
