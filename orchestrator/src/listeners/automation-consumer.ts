/** Stream fallback for automation runs (ADR 0119 D3) and the session relay
 * renderer (contract 5).
 *
 * Mirrors the review consumer: run completion or interruption becomes
 * `session_idle` and a terminal session becomes `session_ended`, both routed
 * to the owning run's mailbox through the automation_session binding. The
 * in-band `signal_automation` tool stays authoritative; this consumer is the
 * backstop that keeps `send_prompt {waitFor: run_end}` and `wait_session`
 * honest when a session never emits a signal.
 *
 * A RELAYED session (an `automation_session_relay` row exists) is also
 * rendered here, event by event, into its place — with no run live. That is
 * what lets a run install the relay and end: the conversation is the
 * session's, not a run's.
 */

import { DBOS, Error as DBOSErrors } from "@dbos-inc/dbos-sdk";

import { makeAutomationEngineStore } from "../db/automations.ts";
import { log as rootLog } from "../log.ts";
import type { CuratedEvent, TerminalOutcome } from "../control-plane/session-events.ts";
import { closeSessionRelay, renderSessionEvent } from "../automations/session-relay.ts";
import {
  AUTOMATION_TOPIC,
  assertIdempotencyKey,
  inboxKeys,
  type AutomationInbox,
} from "../automations/engine/inbox.ts";
import type { SessionConsumer } from "./consumer.ts";

export interface AutomationSessionBindingRef {
  runId: string;
  /** A session relay row exists: the consumer renders this session's events. */
  relay?: boolean;
}

export type AutomationMailboxSend = (
  destinationId: string,
  message: AutomationInbox,
  topic: string,
  idempotencyKey: string,
) => Promise<void>;

/** The session relay's two entry points, injectable for tests. */
export interface AutomationRelayRenderer {
  render(sessionId: string, event: CuratedEvent): Promise<unknown>;
  close(sessionId: string, outcome: TerminalOutcome): Promise<unknown>;
}

export interface AutomationConsumerDeps {
  findSessionBinding(sessionId: string): Promise<AutomationSessionBindingRef | null>;
  send: AutomationMailboxSend;
  relay?: AutomationRelayRenderer;
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

const log = rootLog.child({ component: "automation-consumer" });

export function makeAutomationConsumer(deps: AutomationConsumerDeps): SessionConsumer {
  // The applies-to answer is memoized (a session with no binding never gets
  // one later). The DESTINATION is not: a kept session's binding moves to
  // the next run of its workstream when that run adopts it (D11), and the
  // listener outlives both runs — an address taken once would keep sending
  // every later event to the finished run's mailbox, where it is dropped as
  // "run finished", and the resumed run would never see its session speak.
  let applies: boolean | undefined;

  const destination = async (sessionId: string): Promise<AutomationSessionBindingRef | null> => {
    const live = await deps.findSessionBinding(sessionId);
    if (live === null) {
      log.debug({ sessionId }, "automation session binding gone; session event dropped");
    }
    return live;
  };

  // A session is KEPT by default (D8), so it outlives its run. Every later
  // session event (a follow-up the user types, the idle after it, the
  // eventual end) still maps to the finished run's mailbox, and DBOS rejects
  // a send to a workflow that no longer exists. That is not an error to
  // retry — the run is over and nobody is waiting — so it is a no-op;
  // retrying would pin the listener's cursor on this event forever.
  const deliver: AutomationMailboxSend = async (destinationId, message, topic, idempotencyKey) => {
    try {
      await deps.send(destinationId, message, topic, idempotencyKey);
    } catch (error) {
      if (error instanceof DBOSErrors.DBOSNonExistentWorkflowError) {
        log.debug({ runId: destinationId, kind: message.kind }, "automation run finished; session event dropped");
        return;
      }
      throw error;
    }
  };

  // The relay renders into Slack; a failure there is logged inside the
  // relay and must never pin the listener's cursor on the event.
  const render = async (sessionId: string, event: CuratedEvent): Promise<void> => {
    if (!deps.relay) return;
    try {
      await deps.relay.render(sessionId, event);
    } catch (error) {
      log.error({ sessionId, kind: event.kind, err: error }, "session relay render threw; event dropped");
    }
  };

  return {
    name: "automation",
    interestedIn: () => true,
    async appliesTo(sessionId) {
      if (applies === undefined) {
        applies = (await deps.findSessionBinding(sessionId)) !== null;
      }
      return applies;
    },
    async handle(event, ctx) {
      // Re-read per event: the run that owns the session (adoption moves it)
      // and whether a relay exists (it is installed AFTER the session exists).
      const live = await destination(ctx.sessionId);
      if (live === null) return;
      // Contract 5: a relayed session is rendered into its place, here,
      // with no run live (idle still rides the dedicated arm below, so wait
      // matchers keep working unchanged).
      if (live.relay === true) await render(ctx.sessionId, event);
      // Unlike a review worker, an automation session can go idle once per
      // prompt turn (a kept session takes follow-ups), so the idempotency key
      // carries the event idx — each turn's idle is its own message.
      if (event.kind !== "run_completed" && event.kind !== "run_interrupted") return;
      const runFailed = event.kind === "run_interrupted" || runCompletedFailed(event.payloadJson);
      await deliver(
        live.runId,
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
      const live = await destination(ctx.sessionId);
      if (live === null) return;
      if (live.relay === true && deps.relay) {
        try {
          await deps.relay.close(ctx.sessionId, outcome);
        } catch (error) {
          log.error({ sessionId: ctx.sessionId, outcome, err: error }, "session relay close threw");
        }
      }
      await deliver(
        live.runId,
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
      return binding === null ? null : { runId: binding.runId, relay: binding.relay };
    },
    send: async (destinationId, message, topic, idempotencyKey) => {
      assertIdempotencyKey(idempotencyKey);
      await DBOS.send<AutomationInbox>(destinationId, message, topic, idempotencyKey);
    },
    relay: {
      render: (sessionId, event) => renderSessionEvent(sessionId, event),
      close: (sessionId, outcome) => closeSessionRelay(sessionId, outcome),
    },
  });
}
