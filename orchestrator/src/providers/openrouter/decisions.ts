/** OpenRouter Decisions API client (`POST /api/alpha/decisions`).
 *
 * The API serves System One models, which are not chat models, so the AI SDK
 * provider cannot call it.
 *
 * Wire reference: openrouter.ai/docs/api/api-reference/alphadecisions.
 */

import {
  DecisionsApiError,
  decisionsResponseSchema,
  type DecisionsClient,
} from "../../model-routers/decisions.ts";

/** Pinned so a model release never changes a graph's thresholds; an author
 * opts into a newer one through the request's `model`. */
const DEFAULT_MODEL = "typesafe/jev-1.13";

const RETRYABLE_STATUS = new Set([408, 429, 500, 502, 503, 524, 529]);

export interface DecisionsClientOptions {
  apiKey: string;
  host: string;
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
        res = await doFetch(`https://${options.host}/api/alpha/decisions`, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${options.apiKey}`,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({ ...request, model: request.model ?? DEFAULT_MODEL }),
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
      const parsed = decisionsResponseSchema.safeParse(body);
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
