/**
 * Dev-only smoke tools for ADR 0089 live verification (scenarios A and C).
 * Registered only when ENGRAM_DEV_TOOLS=1 — never in production. `dev_echo`
 * exercises the sync handled path end to end; `dev_echo_deferred` parks the
 * model's call (claude: hook defer; codex: unanswered item/tool/call) and
 * completes it from a timer, so an eviction can happen mid-wait.
 */

import { z } from "zod";

import { log as rootLog } from "../log.ts";
import { tools, type ToolRegistry } from "./registry.ts";

const log = rootLog.child({ component: "dev-tools" });

/** Default wait before a deferred dev echo completes — long enough to
 *  observe (or force) an idle eviction between call and result. */
const DEV_DEFERRED_DELAY_MS = 90_000;

export function registerDevTools(registry: ToolRegistry = tools): void {
  registry.register({
    name: "dev_echo",
    description:
      "Development smoke tool: echoes the given text back immediately.",
    input: z.object({ text: z.string() }),
    output: z.object({ echoed: z.string() }),
    handling: "handled",
    execution: "sync",
    handler: (_ctx, args) => ({ echoed: args.text }),
  });

  registry.register({
    name: "dev_echo_deferred",
    description:
      "Development smoke tool: echoes the given text back after a delay " +
      "(the call is parked; the session may idle or evict meanwhile).",
    input: z.object({
      text: z.string(),
      delaySeconds: z.number().int().positive().optional(),
    }),
    output: z.object({ echoed: z.string() }),
    handling: "handled",
    execution: "deferred",
    handler: (ctx, args) => {
      const delayMs = args.delaySeconds != null ? args.delaySeconds * 1_000 : DEV_DEFERRED_DELAY_MS;
      // A plain timer is enough for a dev smoke: an orchestrator restart
      // mid-wait loses it, which the §1 watchdog would eventually flag.
      setTimeout(() => {
        registry.complete(ctx.sessionId, ctx.toolCallId, { echoed: args.text }).catch((error) => {
          log.error(
            { sessionId: ctx.sessionId, toolCallId: ctx.toolCallId, error: String(error) },
            "dev_echo_deferred completion failed",
          );
        });
      }, delayMs);
    },
  });
}
