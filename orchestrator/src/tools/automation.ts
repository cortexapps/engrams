/** In-band signals from automation worker sessions (ADR 0119 D3).
 *
 * `signal_automation` is the authoritative "this phase is done" channel for
 * `send_prompt {waitFor: signal:<name>}` waits — the built-in review pipeline
 * layers `finder_done`/`verifier_done` on top of it in phase 4. The stream
 * consumer (listeners/automation-consumer.ts) is the run_end backstop.
 *
 * Scoped by task type, not capability: every automation-launched session has
 * taskType "automation" (rpc/task-create.ts), and no other session kind does.
 */

import { z } from "zod";
import { DBOS } from "@dbos-inc/dbos-sdk";

import { makeAutomationEngineStore } from "../db/automations.ts";
import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  inboxKeys,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import { log as rootLog } from "../log.ts";
import { tools, type ToolRegistry } from "./registry.ts";

const log = rootLog.child({ component: "automation-tools" });

const SIGNAL_NAME_RE = /^[a-z][a-z0-9_]*$/;
const MAX_PAYLOAD_CHARS = 16 * 1024;

export type AutomationSignalNotify = (
  destinationId: string,
  message: AutomationInbox,
  topic: string,
  idempotencyKey: string,
) => Promise<void>;

export interface AutomationToolDeps {
  findSessionBinding?(sessionId: string): Promise<{ runId: string } | null>;
  notify?: AutomationSignalNotify;
}

export function registerAutomationTools(
  registry: ToolRegistry = tools,
  deps?: AutomationToolDeps,
): void {
  let engineStore: ReturnType<typeof makeAutomationEngineStore> | undefined;
  const findBinding =
    deps?.findSessionBinding ??
    (async (sessionId: string) => {
      engineStore ??= makeAutomationEngineStore();
      const binding = await engineStore.findSessionBinding(sessionId);
      return binding === null ? null : { runId: binding.runId };
    });
  const notify: AutomationSignalNotify =
    deps?.notify ??
    (async (destinationId, message, topic, idempotencyKey) => {
      assertIdempotencyKey(idempotencyKey);
      await DBOS.send<AutomationInbox>(destinationId, message, topic, idempotencyKey);
    });

  registry.register({
    name: "signal_automation",
    description:
      "Signal the automation run that owns this session. Use the signal name the run's instructions gave you (for example when a requested phase of work is complete), with an optional small JSON payload of results.",
    input: z.object({
      signal: z.string().regex(SIGNAL_NAME_RE).max(64),
      payload: z.record(z.string(), z.unknown()).optional(),
    }),
    output: z.object({ delivered: z.boolean() }),
    handling: "handled",
    execution: "sync",
    taskTypes: ["automation"],
    handler: async (ctx, args) => {
      if (args.payload !== undefined && JSON.stringify(args.payload).length > MAX_PAYLOAD_CHARS) {
        return { error: `payload larger than ${MAX_PAYLOAD_CHARS} characters` };
      }
      const binding = await findBinding(ctx.sessionId);
      if (binding === null) {
        // A session outside any run (e.g. kept after its run ended) can still
        // carry the tool; the signal has nowhere to go and says so.
        return { delivered: false };
      }
      try {
        await notify(
          binding.runId,
          {
            kind: "signal",
            name: args.signal,
            sessionId: ctx.sessionId,
            ...(args.payload !== undefined ? { payload: args.payload } : {}),
          },
          AUTOMATION_TOPIC,
          inboxKeys.signal(ctx.sessionId, args.signal, ctx.toolCallId),
        );
      } catch (err) {
        // The tool result is the session's record; a notification outage must
        // not fail the tool — the stream fallback and wait deadlines cover it.
        log.warn(
          { sessionId: ctx.sessionId, signal: args.signal, err },
          "automation signal notification failed",
        );
        return { delivered: false };
      }
      return { delivered: true };
    },
  });
}
