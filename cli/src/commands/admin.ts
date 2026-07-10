/**
 * engrams admin … — explicit triggers for primitives whose production driver
 * is implicit (the idle detector, the flush scanner). Same pipelines, no TTL
 * wait. Mirrors the retired Rust CLI's `admin {flush,evict-idle}`.
 */

import type { Clients } from "../client.ts";
import { failWith, printJson } from "../output.ts";

/** FleetService.FlushSession — force a chunked-disk flush of one session. */
export async function flush(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.fleet.flushSession({ sessionId: id }).catch(failWith);
  if (json) {
    printJson({
      outcome: resp.outcome,
      manifest_version:
        resp.manifestVersion !== undefined ? Number(resp.manifestVersion) : undefined,
    });
    return;
  }
  console.log(
    resp.manifestVersion !== undefined
      ? `${resp.outcome}: manifest_version=${resp.manifestVersion}`
      : resp.outcome,
  );
}

/** SessionService.EvictIdle — the idle-eviction pipeline, now. Synchronous:
 *  the session is Idle by the time this returns. */
export async function evictIdle(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.session.evictIdle({ sessionId: id }).catch(failWith);
  if (json) printJson({ session_id: resp.sessionId, status: resp.status });
  else console.log(`${resp.sessionId}: ${resp.status}`);
}
