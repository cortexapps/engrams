/** Session-facing blocks: create_session, send_prompt, wait_session,
 * end_session (ADR 0119 D1, D8).
 *
 * All control-plane work goes through the injected EngineSessionOps. Sessions
 * a run creates are KEPT by default; teardown is the end_session block or the
 * automation's end_sessions_on_finish setting (honored by finalize).
 */

import { z } from "zod";

import {
  appendAutomationEventContext,
  AUTOMATION_OUTPUT_MAX_CHARS,
} from "../../template.ts";
import { sessionRefSchema } from "../definition.ts";
import type { RunContext } from "../context.ts";
import type { AutomationInbox } from "../inbox.ts";
import { registerBlock, type BlockOutcome } from "./registry.ts";

const overrideFields = {
  harnessMode: z.string().min(1).optional(),
  harness: z.string().min(1).optional(),
  model: z.string().min(1).optional(),
  modelRouter: z.string().min(1).optional(),
  effort: z.string().min(1).optional(),
};

/** Session-policy clamps a block may impose on top of the profile (phase 4.3:
 * the built-in review workers). Strings inside may carry Liquid templates. */
const sessionPolicyFields = {
  /** Replaces the profile's integration grants outright. */
  capabilityOverride: z.array(z.string().min(1)).optional(),
  networkOverride: z
    .object({
      default: z.enum(["deny", "allow"]),
      allowHosts: z.array(z.string().min(1)),
      allowHostPatterns: z.array(z.string().min(1)),
    })
    .optional(),
  dropProfileSecretsAndEnv: z.boolean().optional(),
  appendSystemPrompt: z.string().optional(),
};

export const createSessionConfigSchema = z.object({
  profileId: z.string().min(1),
  promptTemplate: z.string(),
  titleTemplate: z.string().optional(),
  includeEventContext: z.boolean().optional(),
  role: z.string().min(1).max(64).optional(),
  /** Default: keep (D8). finalize ends only keep=false sessions. */
  keepOnFinish: z.boolean().optional(),
  ...overrideFields,
  ...sessionPolicyFields,
});
export type CreateSessionConfig = z.infer<typeof createSessionConfigSchema>;

export const sendPromptConfigSchema = z.object({
  session: sessionRefSchema,
  promptTemplate: z.string().min(1),
  waitFor: z.union([
    z.object({ kind: z.literal("run_end") }),
    z.object({ kind: z.literal("signal"), name: z.string().regex(/^[a-z][a-z0-9_]*$/) }),
    z.object({ kind: z.literal("none") }),
  ]),
  deadlineSeconds: z.number().int().min(1).max(24 * 3600).optional(),
  harnessMode: z.string().min(1).optional(),
});
export type SendPromptConfig = z.infer<typeof sendPromptConfigSchema>;

export const waitSessionConfigSchema = z.object({
  session: sessionRefSchema,
  until: z.enum(["idle", "ended"]),
  deadlineSeconds: z.number().int().min(1).max(24 * 3600).optional(),
});
export type WaitSessionConfig = z.infer<typeof waitSessionConfigSchema>;

export const endSessionConfigSchema = z.object({ session: sessionRefSchema });
export type EndSessionConfig = z.infer<typeof endSessionConfigSchema>;

const DEFAULT_WAIT_DEADLINE_S = 7200;

async function executeCreateSession(
  config: CreateSessionConfig,
  ctx: RunContext,
): Promise<BlockOutcome> {
  let prompt = await ctx.render(config.promptTemplate);
  // ADR 0102 parity: includeEventContext appends the redacted payload with
  // the untrusted-data disclaimer, exactly as renderAutomationAction did on
  // the legacy run path. trigger.event is the event key; a cron/manual run
  // has none and appends nothing (the legacy gate).
  if (config.includeEventContext && typeof ctx.trigger.event === "string") {
    prompt = appendAutomationEventContext(prompt, {
      automationName: ctx.automationName,
      eventKey: ctx.trigger.event,
      redactedPayload: ctx.event.raw,
    });
    if (prompt.length > AUTOMATION_OUTPUT_MAX_CHARS) {
      return {
        kind: "error",
        code: "output_too_long",
        message: `prompt with event context is ${prompt.length} characters (max ${AUTOMATION_OUTPUT_MAX_CHARS})`,
        retryable: false,
      };
    }
  }
  const title = config.titleTemplate !== undefined ? await ctx.render(config.titleTemplate) : null;
  // Profile and capability strings may be templated (`${{ inputs.profile }}`,
  // `github:contents:read@${{ event.repository.full_name }}`).
  const profileId = await renderIfTemplated(ctx, config.profileId);
  const capabilityOverride =
    config.capabilityOverride !== undefined
      ? await Promise.all(config.capabilityOverride.map((c) => renderIfTemplated(ctx, c)))
      : undefined;
  const created = await ctx.deps.sessions.createSession({
    runId: ctx.runId,
    blockId: currentBlockId(ctx),
    automationId: ctx.automationId,
    profileId,
    prompt,
    title,
    role: config.role ?? "primary",
    // D8: keep by default; end_sessions_on_finish flips the default, and the
    // block's keepOnFinish overrides either way.
    keep: config.keepOnFinish ?? !ctx.settings.endSessionsOnFinish,
    ...(config.harnessMode !== undefined ? { harnessMode: config.harnessMode } : {}),
    ...(config.harness !== undefined ? { harness: config.harness } : {}),
    ...(config.model !== undefined ? { model: config.model } : {}),
    ...(config.modelRouter !== undefined ? { modelRouter: config.modelRouter } : {}),
    ...(config.effort !== undefined ? { effort: config.effort } : {}),
    ...(capabilityOverride !== undefined ? { capabilityOverride } : {}),
    ...(config.networkOverride !== undefined ? { networkOverride: config.networkOverride } : {}),
    ...(config.dropProfileSecretsAndEnv !== undefined
      ? { dropProfileSecretsAndEnv: config.dropProfileSecretsAndEnv }
      : {}),
    ...(config.appendSystemPrompt !== undefined
      ? { appendSystemPrompt: config.appendSystemPrompt }
      : {}),
  });
  return {
    kind: "ok",
    // initial_prompt feeds the interpreter's turn ledger: an empty initial
    // prompt starts no harness run, so it must not count as turn 1.
    outputs: {
      session_id: created.sessionId,
      task_id: created.taskId,
      initial_prompt: prompt.length > 0,
    },
  };
}

async function renderIfTemplated(ctx: RunContext, value: string): Promise<string> {
  return value.includes("${{") ? ctx.render(value) : value;
}

function currentBlockId(ctx: RunContext): string {
  if (ctx.currentBlockId === undefined) throw new Error("engine bug: currentBlockId missing");
  return ctx.currentBlockId;
}

function matchesSessionMessage(
  msg: AutomationInbox,
  sessionId: string,
  waitFor: SendPromptConfig["waitFor"] | { kind: "idle" } | { kind: "ended" },
): Record<string, unknown> | "ignore" | null {
  switch (msg.kind) {
    case "session_idle":
      if (msg.sessionId !== sessionId) return null;
      if (waitFor.kind === "run_end" || waitFor.kind === "idle") {
        return { outcome: msg.runFailed ? "failed" : "completed" };
      }
      return "ignore";
    case "session_ended":
      if (msg.sessionId !== sessionId) return null;
      // A terminal session satisfies every session wait: nothing later can.
      return { outcome: msg.outcome === "completed" ? "completed" : "failed", ended: true };
    case "signal":
      if (waitFor.kind !== "signal") return null;
      if (msg.sessionId !== undefined && msg.sessionId !== sessionId) return null;
      if (msg.name !== waitFor.name) return null;
      return { outcome: "completed", signal: msg.payload ?? {} };
    default:
      return null;
  }
}

export function registerSessionBlocks(): void {
  registerBlock<CreateSessionConfig>({
    type: "create_session",
    outputs: ["session_id", "task_id"],
    configSchema: createSessionConfigSchema,
    execute: executeCreateSession,
  });

  registerBlock<SendPromptConfig>({
    type: "send_prompt",
    outputs: ["outcome", "signal"],
    configSchema: sendPromptConfigSchema,
    requiresSession: true,
    async execute(config, ctx) {
      const sessionId = await ctx.resolveSession(config.session);
      const text = await ctx.render(config.promptTemplate);
      const promptId = `autorun:${ctx.runId}:${currentBlockId(ctx)}:${sessionId}`;
      await ctx.deps.sessions.sendPrompt(sessionId, promptId, text, config.harnessMode);
      return { kind: "ok", outputs: { session_id: sessionId, sent: true } };
    },
    wait: {
      deadlineSeconds(config) {
        if (config.waitFor.kind === "none") return 0;
        return config.deadlineSeconds ?? DEFAULT_WAIT_DEADLINE_S;
      },
      matches(msg, config, ctx) {
        const sessionId = sessionIdFromOwnStep(ctx);
        if (sessionId === undefined) return null;
        return matchesSessionMessage(msg, sessionId, config.waitFor);
      },
      onDeadline() {
        return { outcome: "deadline" };
      },
    },
  });

  registerBlock<WaitSessionConfig>({
    type: "wait_session",
    outputs: ["outcome"],
    configSchema: waitSessionConfigSchema,
    requiresSession: true,
    async execute(config, ctx) {
      const sessionId = await ctx.resolveSession(config.session);
      return { kind: "ok", outputs: { session_id: sessionId } };
    },
    wait: {
      deadlineSeconds(config) {
        return config.deadlineSeconds ?? DEFAULT_WAIT_DEADLINE_S;
      },
      matches(msg, config, ctx) {
        const sessionId = sessionIdFromOwnStep(ctx);
        if (sessionId === undefined) return null;
        return matchesSessionMessage(msg, sessionId, { kind: config.until });
      },
      onDeadline() {
        return { outcome: "deadline" };
      },
    },
  });

  registerBlock<EndSessionConfig>({
    type: "end_session",
    configSchema: endSessionConfigSchema,
    requiresSession: true,
    async execute(config, ctx) {
      const sessionId = await ctx.resolveSession(config.session);
      await ctx.deps.sessions.endSession(sessionId);
      return { kind: "ok", outputs: { session_id: sessionId, ended: true } };
    },
  });
}

/** A wait's matcher runs after its own execute step recorded outputs; the
 * resolved session id is read back from the block's own outputs so matching
 * is a pure function of checkpointed state. */
function sessionIdFromOwnStep(ctx: RunContext): string | undefined {
  if (ctx.currentBlockId === undefined) return undefined;
  const value = ctx.steps[ctx.currentBlockId]?.["session_id"];
  return typeof value === "string" ? value : undefined;
}
