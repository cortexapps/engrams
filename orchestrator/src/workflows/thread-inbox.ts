/**
 * The thread workflow's single mailbox contract (ADR 0059 P1.4).
 *
 * `DBOS.recv` is single-topic, so the ADR's "one recv multiplexes
 * { session events ∪ trigger events }" is realized by funnelling BOTH sources
 * onto ONE topic with a tagged message: the per-session pump
 * (`SessionIngestWorkflow`) sends the `session_*` variants, and the trigger
 * HTTP handlers (P2) send the `trigger_*` variants — all to the thread
 * workflow's id on `THREAD_TOPIC`. The thread workflow switches on `kind`.
 */

import type { CuratedEvent } from "../control-plane/session-events.ts";

/** The one topic the thread workflow `recv`s; every sender targets it. */
export const THREAD_TOPIC = "thread";

/**
 * An `@mention` — the initial trigger or a follow-up. The provider-shaped
 * fields are opaque to the framework; only the source's `CommunicationPolicy`
 * interprets them. Shape matches the Slack events handler (ADR 0059 §handler).
 */
export interface SourceMention {
  team: string;
  channel: string;
  threadRoot: string;
  user: string;
  ts: string;
  eventId: string;
}

/**
 * A human answer to a deferred `AskUserQuestion`, arriving via the source's
 * interactivity surface (P2). `answers` is keyed by question text → selected
 * labels, matching `AnswerQuestionRequest.answers` (ADR 0054).
 */
export interface SourceAnswer {
  toolCallId: string;
  answers: Record<string, string[]>;
}

/** Everything the thread workflow can receive, tagged by origin. */
export type ThreadInbox =
  | { kind: "session_event"; event: CuratedEvent }
  | { kind: "session_terminal"; ok: boolean }
  | { kind: "trigger_mention"; mention: SourceMention }
  | { kind: "trigger_answer"; answer: SourceAnswer };
