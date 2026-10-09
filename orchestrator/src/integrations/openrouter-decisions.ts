/**
 * OpenRouter Decisions API client (`POST /api/alpha/decisions`).
 *
 * The Decisions API serves System One models (TypeSafe's Jev): a `state`
 * plus typed `questions` in, calibrated typed answers out. It is NOT a chat
 * model, so the AI SDK provider in ./openrouter.ts cannot call it; this is a
 * thin `fetch` client on the same `openrouter.api_key` credential.
 *
 * Wire reference: openrouter.ai/docs/api/api-reference/alphadecisions.
 */

import { z } from "zod";

export const DECISIONS_URL = "https://openrouter.ai/api/alpha/decisions";

/** A JSON value the API accepts as `state`, `instructions` or a criterion. */
export type DecisionText = string | Record<string, unknown> | unknown[];

export type DecisionQuestion =
  | { type: "noul"; instructions: DecisionText; criteria?: { true: DecisionText; false: DecisionText } }
  | { type: "choice"; instructions: DecisionText; criteria: Record<string, DecisionText | null> }
  | { type: "score"; instructions: DecisionText; criteria: DecisionText[] };

export interface DecisionsRequest {
  model: string;
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

const responseSchema = z.object({
  id: z.string().optional(),
  model: z.string(),
  answers: z.record(z.string(), answerSchema),
  usage: z.object({
    input_tokens: z.number(),
    output_tokens: z.number(),
    cost: z.number().optional(),
  }),
});
export type DecisionsResponse = z.infer<typeof responseSchema>;

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

const RETRYABLE_STATUS = new Set([408, 429, 500, 502, 503, 524, 529]);

export interface DecisionsClient {
  decide(request: DecisionsRequest): Promise<DecisionsResponse>;
}

export interface DecisionsClientOptions {
  apiKey: string;
  /** Per-call abort. The model answers in well under a second; a call that
   *  hangs is a provider failure the caller's retry policy handles. */
  timeoutMs?: number;
  /** The one call this client makes; tests substitute a fake. */
  fetch?: (url: string, init: RequestInit) => Promise<Response>;
}

const DEFAULT_TIMEOUT_MS = 5_000;

export function makeDecisionsClient(options: DecisionsClientOptions): DecisionsClient {
  const doFetch = options.fetch ?? fetch;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  return {
    async decide(request) {
      let res: Response;
      let text: string;
      try {
        res = await doFetch(DECISIONS_URL, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${options.apiKey}`,
            "Content-Type": "application/json",
          },
          body: JSON.stringify(request),
          signal: AbortSignal.timeout(timeoutMs),
        });
        // The body read is part of the transport: a reset or the abort after
        // the headers arrived is as transient as one before them.
        text = await res.text();
      } catch (error) {
        // Network failure or the abort: transient by definition.
        throw new DecisionsApiError(
          `decisions request failed: ${error instanceof Error ? error.message : String(error)}`,
          null,
          true,
        );
      }
      if (!res.ok) {
        throw new DecisionsApiError(
          `decisions API ${res.status}: ${errorMessage(text)}`,
          res.status,
          RETRYABLE_STATUS.has(res.status),
        );
      }
      let body: unknown;
      try {
        body = JSON.parse(text);
      } catch {
        // A 200 with a non-JSON body (an HTML fallback page) is an upstream
        // fault, not a bad request.
        throw new DecisionsApiError("decisions API returned a non-JSON body", res.status, true);
      }
      const parsed = responseSchema.safeParse(body);
      if (!parsed.success) {
        const issue = parsed.error.issues[0];
        throw new DecisionsApiError(
          `decisions API response invalid at ${issue ? issue.path.join(".") : "body"}: ${issue?.message ?? "unknown"}`,
          res.status,
          false,
        );
      }
      return parsed.data;
    },
  };
}

/** The `error.message` of a documented error body, else a bounded raw slice. */
function errorMessage(text: string): string {
  try {
    const body = JSON.parse(text) as { error?: { message?: unknown } };
    if (typeof body.error?.message === "string") return body.error.message;
  } catch {
    // fall through
  }
  return text.slice(0, 200);
}
