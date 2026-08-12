/**
 * The REST client for the spec surface.
 *
 * Spec reads, history, and creation are Hono routes rather than Connect rpcs,
 * so they share one request helper. The error carries both the status (which
 * the pages branch on) and the server's own message (which they show).
 */

import { API_BASE } from "@/lib/base";

export class SpecRequestError extends Error {
  constructor(
    readonly path: string,
    readonly status: number,
    detail?: string,
    /** The parsed JSON body, when the handler answered with one. A refusal
     *  that carries its own state (the publish gate) is read from here. */
    readonly body?: unknown,
  ) {
    super(detail && detail.length > 0 ? detail : `${path} → ${status}`);
    this.name = "SpecRequestError";
  }
}

export async function specRequest<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${path}`, {
    credentials: "include",
    headers: { Accept: "application/json", ...init?.headers },
    ...init,
  });
  if (!response.ok) {
    const failure = await errorDetail(response);
    throw new SpecRequestError(path, response.status, failure.detail, failure.body);
  }
  return response.json() as Promise<T>;
}

/** Hono's HTTPException answers with plain text; a handler may answer JSON. */
async function errorDetail(response: Response): Promise<{ detail: string; body?: unknown }> {
  let raw: string;
  try {
    raw = await response.text();
  } catch {
    return { detail: "" };
  }
  try {
    const body: unknown = JSON.parse(raw);
    if (typeof body === "object" && body !== null) {
      const named = body as { message?: unknown; error?: unknown };
      const detail = typeof named.message === "string" ? named.message : undefined;
      const fallback = typeof named.error === "string" ? named.error : raw;
      return { detail: detail ?? fallback, body };
    }
  } catch {
    // Plain text is the normal shape.
  }
  return { detail: raw };
}
