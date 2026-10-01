/** Browser-facing links under the orchestrator's public URL.
 *
 * `config.baseUrl` is operator input (`ORCHESTRATOR_PUBLIC_URL`) and may
 * carry a trailing slash. One strip here keeps every link one shape, so a
 * thread's "Started a session" message and the relay's asset links point at
 * the same URL. */

import { config } from "./config.ts";

export function publicUrl(path: string): string {
  return `${config.baseUrl.replace(/\/$/, "")}${path}`;
}

/** The session page a human opens. */
export function sessionWebUrl(sessionId: string): string {
  return publicUrl(`/sessions/${sessionId}`);
}
