/** The run mailbox (ADR 0119 D3).
 *
 * One topic; the DBOS workflow id (= the run id) addresses the mailbox. Every
 * send carries a NON-EMPTY idempotency key: DBOS's notifications table
 * conflicts on the message id alone, so one empty-string key would swallow
 * every later empty-key send system-wide (the ADR 0060 hazard, guarded the
 * same way as `defaultDbos` in workflows/dispatch-review.ts).
 */

import { DBOS } from "@dbos-inc/dbos-sdk";

import type { TerminalOutcome } from "../../control-plane/session-events.ts";

export const AUTOMATION_TOPIC = "automation";

export type AutomationInbox =
  | { kind: "session_idle"; sessionId: string; runFailed?: boolean }
  | { kind: "session_ended"; sessionId: string; outcome: TerminalOutcome }
  | { kind: "signal"; name: string; sessionId?: string; payload?: Record<string, unknown> }
  | {
      kind: "event";
      eventKey: string;
      deliveryKey: string;
      payload: Record<string, unknown>;
      receivedAt: string;
    }
  | { kind: "stop"; reason?: string }
  | { kind: "supersede"; byRunId: string };

export interface AutomationSender {
  send(runId: string, message: AutomationInbox, idempotencyKey: string): Promise<void>;
}

export function assertIdempotencyKey(key: string): void {
  if (key === "") {
    throw new Error(
      "empty DBOS idempotency key: it would be deduplicated against every other empty-key send",
    );
  }
}

export const defaultAutomationSender: AutomationSender = {
  async send(runId, message, idempotencyKey) {
    assertIdempotencyKey(idempotencyKey);
    await DBOS.send<AutomationInbox>(runId, message, AUTOMATION_TOPIC, idempotencyKey);
  },
};

/** Idempotency-key builders. Each key is unique per logical occurrence. */
export const inboxKeys = {
  sessionIdle: (sessionId: string, eventIdx: bigint | number): string =>
    `autorun:${sessionId}:idle:${eventIdx}`,
  sessionEnded: (sessionId: string): string => `autorun:${sessionId}:terminal`,
  signal: (sessionId: string, name: string, toolCallId: string): string =>
    `autorun:${sessionId}:signal:${name}:${toolCallId}`,
  joinedEvent: (deliveryKey: string, runId: string): string =>
    `autorun-evt:${deliveryKey}:${runId}`,
  stop: (runId: string, requestId: string): string => `autorun:${runId}:stop:${requestId}`,
  supersede: (runId: string, byRunId: string): string =>
    `autorun:${runId}:supersede:${byRunId}`,
} as const;
