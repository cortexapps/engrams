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
    throw new SpecRequestError(path, response.status, await errorDetail(response));
  }
  return response.json() as Promise<T>;
}

/** Hono's HTTPException answers with plain text; a handler may answer JSON. */
async function errorDetail(response: Response): Promise<string> {
  let raw: string;
  try {
    raw = await response.text();
  } catch {
    return "";
  }
  try {
    const body: unknown = JSON.parse(raw);
    if (typeof body === "object" && body !== null && "message" in body) {
      const message = (body as { message: unknown }).message;
      if (typeof message === "string") return message;
    }
  } catch {
    // Plain text is the normal shape.
  }
  return raw;
}
