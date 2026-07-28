/** The PrReviewWorkflow's single mailbox contract (ADR 0100). */

import type { TerminalOutcome } from "../control-plane/session-events.ts";
export const REVIEW_TOPIC = "review";

export type ReviewInbox =
  | {
      kind: "trigger";
      // ADR 0100 decision 11: ingress resolves the change BEFORE this message is
      // sent, so every field here is required. The review workflow can no longer
      // reach a state where it does not know what it is reviewing — that state is
      // gone, not handled.
      reviewId: string;
      taskId: string;
      repo: string;
      prNumber: number;
      trigger: string;
      headSha: string;
      baseSha: string;
      focus?: string;
    }
  | { kind: "comment"; commentId: string; body: string }
  | { kind: "phase_done"; role: string }
  // `session_idle` is the backup completion signal: the harness run ended and
  // the reusable session returned to idle. `runFailed` distinguishes a run that
  // ERRORED (e.g. the agent could not authenticate) from a clean turn — a
  // failed run must never be reported to a PR as a "no findings" completion.
  | { kind: "session_idle"; role: string; runFailed?: boolean }
  | {
      kind: "session_ended";
      role: string;
      sessionId: string;
      outcome: TerminalOutcome;
    }
  | { kind: "stop" }
  | { kind: "supersede" };
