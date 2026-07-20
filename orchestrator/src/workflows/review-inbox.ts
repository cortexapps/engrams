/** The PrReviewWorkflow's single mailbox contract (ADR 0100). */

import type { TerminalOutcome } from "../control-plane/session-events.ts";

export const REVIEW_TOPIC = "review";

export type ReviewInbox =
  | {
      kind: "trigger";
      // The first message must carry the unhashed identity: the deterministic
      // workflow id cannot be reversed when the durable review row is created.
      repo: string;
      prNumber: number;
      trigger: string;
      headSha?: string;
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
  | { kind: "stop" };
