/** The PrReviewWorkflow's single mailbox contract (ADR 0100). */

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
  | { kind: "stop" };
