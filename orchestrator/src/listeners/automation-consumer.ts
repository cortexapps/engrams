/** Stream fallback for automation runs (ADR 0119 D3).
 *
 * Mirrors the review consumer: `run_completed` becomes `session_idle` and a
 * terminal session becomes `session_ended`, both routed to the owning run's
 * mailbox through the automation_session binding. The in-band
 * `signal_automation` tool stays authoritative; this consumer is the backstop
 * that keeps `send_prompt {waitFor: run_end}` and `wait_session` honest when
 * a session never emits a signal.
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { makeAutomationEngineStore } from "../db/automations.ts";
import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  inboxKeys,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import type { SessionConsumer } from "./consumer.ts";

export interface AutomationSessionBindingRef {
  runId: string;
}

export type AutomationMailboxSend = (
  destinationId: string,
  message: AutomationInbox,
  topic: string,
  idempotencyKey: string,
) => Promise<void>;

export interface AutomationConsumerDeps {
  findSessionBinding(sessionId: string): Promise<AutomationSessionBindingRef | null>;
  send: AutomationMailboxSend;
}

/** `ok:false` marks a harness run that ERRORED; a malformed or absent flag is
 * a clean run — the same tolerant default the review consumer uses. */
function runCompletedFailed(payloadJson: string): boolean {
  try {
    return (JSON.parse(payloadJson) as { ok?: unknown }).ok === false;
  } catch {
    return false;
  }
}

export function makeAutomationConsumer(deps: AutomationConsumerDeps): SessionConsumer {
  let binding: AutomationSessionBindingRef | null | undefined;

  const destination = (): AutomationSessionBindingRef => {
    if (!binding) throw new Error("Automation consumer has no run binding");
    return binding;
  };

  return {
    name: "automation",
    interestedIn: () => true,
    async appliesTo(sessionId) {
      if (binding === undefined) {
        binding = await deps.findSessionBinding(sessionId);
      }
      return binding !== null;
    },
    async handle(event, ctx) {
      // Unlike a review worker, an automation session can go idle once per
      // prompt turn (a kept session takes follow-ups), so the idempotency key
      // carries the event idx — each turn's idle is its own message.
      if (event.kind !== "run_completed") return;
      const runFailed = runCompletedFailed(event.payloadJson);
      await deps.send(
        destination().runId,
        {
          kind: "session_idle",
          sessionId: ctx.sessionId,
          ...(runFailed ? { runFailed: true } : {}),
        },
        AUTOMATION_TOPIC,
        inboxKeys.sessionIdle(ctx.sessionId, event.idx),
      );
    },
    async onTerminal(outcome, ctx) {
      await deps.send(
        destination().runId,
        { kind: "session_ended", sessionId: ctx.sessionId, outcome },
        AUTOMATION_TOPIC,
        inboxKeys.sessionEnded(ctx.sessionId),
      );
    },
  };
}

export function makeProductionAutomationConsumer(): SessionConsumer {
  const store = makeAutomationEngineStore();
  return makeAutomationConsumer({
    findSessionBinding: async (sessionId) => {
      const binding = await store.findSessionBinding(sessionId);
      return binding === null ? null : { runId: binding.runId };
    },
    send: async (destinationId, message, topic, idempotencyKey) => {
      assertIdempotencyKey(idempotencyKey);
      await DBOS.send<AutomationInbox>(destinationId, message, topic, idempotencyKey);
    },
  });
}
