/** The decisions contract: a `state` plus typed `questions` in, typed
 * answers out. A router answers it through a native endpoint when it has
 * one, and through a structured chat completion when it does not.
 */

import { z } from "zod";

/** A JSON value the API accepts as `state`, `instructions` or a criterion. */
export type DecisionText = string | Record<string, unknown> | unknown[];

export type DecisionQuestion =
  | { type: "noul"; instructions: DecisionText; criteria?: { true: DecisionText; false: DecisionText } }
  | { type: "choice"; instructions: DecisionText; criteria: Record<string, DecisionText | null> }
  | { type: "score"; instructions: DecisionText; criteria: DecisionText[] };

export interface DecisionsRequest {
  /** Default: the router's decision model. */
  model?: string;
  state: DecisionText;
  questions: Record<string, DecisionQuestion>;
}

const answerSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("noul"), noul: z.number() }),
  z.object({
    type: z.literal("choice"),
    choice: z.string(),
    confidence: z.number().optional(),
    probabilities: z.record(z.string(), z.number()).optional(),
  }),
  z.object({
    type: z.literal("score"),
    score: z.number(),
    confidence: z.number().optional(),
    probabilities: z.record(z.string(), z.number()).optional(),
    legend: z.record(z.string(), z.unknown()).optional(),
  }),
]);
export type DecisionAnswer = z.infer<typeof answerSchema>;

export const decisionsResponseSchema = z.object({
  id: z.string().optional(),
  model: z.string(),
  answers: z.record(z.string(), answerSchema),
  usage: z.object({
    input_tokens: z.number(),
    output_tokens: z.number(),
    cost: z.number().optional(),
  }),
});
export type DecisionsResponse = z.infer<typeof decisionsResponseSchema>;

/** A failed decisions call. `retryable` follows the documented status codes:
 * rate limits, upstream/provider failures and timeouts are worth a retry; a
 * bad request, a bad key, no credits or a too-large payload are not. */
export class DecisionsApiError extends Error {
  constructor(
    message: string,
    readonly status: number | null,
    readonly retryable: boolean,
  ) {
    super(message);
    this.name = "DecisionsApiError";
  }
}

export interface DecisionsClient {
  decide(request: DecisionsRequest & { model: string }): Promise<DecisionsResponse>;
}
