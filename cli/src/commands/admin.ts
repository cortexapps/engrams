/**
 * engrams admin … — explicit triggers for primitives whose production driver
 * is implicit (the idle detector, the flush scanner). Same pipelines, no TTL
 * wait. Mirrors the retired Rust CLI's `admin {flush,evict-idle}`.
 */

import type { Clients } from "../client.ts";
import { errorMessage, fail, failWith, printJson } from "../output.ts";

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

/** The three blob-storage GC sweeps (bundle generations / snapshot blobs /
 *  chunks — ADR 0035 §5 / ADR 0028 addendum / ADR 0016 Phase C). The proto's
 *  bare `dry_run: false` means a LIVE sweep with deletions, so this verb
 *  inverts the polarity: dry-run unless `apply`. `graceSecs === 0n` promotes
 *  (deletes) candidates in the same sweep — the dev "clear it now" posture.
 *  The sweeps are independent: each reports as it lands, failures are
 *  collected so one wedged sweep can't swallow the others' reports. */
export async function gc(
  c: Clients,
  apply: boolean,
  graceSecs: bigint | undefined,
  json: boolean,
): Promise<void> {
  const dryRun = !apply;
  const mode = dryRun ? "dry-run" : "applied";
  const failures: string[] = [];
  const out: Record<string, unknown> = { dry_run: dryRun };

  try {
    const b = await c.fleet.bundleGc({ dryRun, graceSecs });
    if (json) {
      out.bundle = {
        listed: Number(b.listed),
        pinned: Number(b.pinSetSize),
        candidates_marked: Number(b.candidatesMarked),
        promoted_deletes: Number(b.promotedDeletes),
        promote_delete_errors: Number(b.promoteDeleteErrors),
      };
    } else {
      console.log(
        `bundle-gc (${mode}): listed=${b.listed} pinned=${b.pinSetSize} candidates=${b.candidatesMarked} deleted=${b.promotedDeletes} errors=${b.promoteDeleteErrors}`,
      );
    }
  } catch (e) {
    failures.push(`bundle-gc: ${errorMessage(e)}`);
  }

  try {
    const s = await c.fleet.snapshotBlobGc({ dryRun, graceSecs });
    if (json) {
      out.snapshot_blob = {
        listed: Number(s.listed),
        pinned: Number(s.pinSetSize),
        candidates_marked: Number(s.candidatesMarked),
        promoted_deletes: Number(s.promotedDeletes),
        promote_repinned_skips: Number(s.promoteRepinnedSkips),
        promote_delete_errors: Number(s.promoteDeleteErrors),
      };
    } else {
      console.log(
        `snapshot-blob-gc (${mode}): listed=${s.listed} pinned=${s.pinSetSize} candidates=${s.candidatesMarked} deleted=${s.promotedDeletes} repinned_skips=${s.promoteRepinnedSkips} errors=${s.promoteDeleteErrors}`,
      );
    }
  } catch (e) {
    failures.push(`snapshot-blob-gc: ${errorMessage(e)}`);
  }

  try {
    const k = await c.fleet.chunkGc({ dryRun, graceSecs });
    if (json) {
      out.chunk = {
        listed: Number(k.listedChunks),
        pinned: Number(k.pinSetSize),
        candidates_marked: Number(k.candidatesMarked),
        promoted_deletes: Number(k.promotedDeletes),
        promote_delete_errors: Number(k.promoteDeleteErrors),
        grace_secs: Number(k.graceSecs),
      };
    } else {
      console.log(
        `chunk-gc (${mode}): listed=${k.listedChunks} pinned=${k.pinSetSize} candidates=${k.candidatesMarked} deleted=${k.promotedDeletes} errors=${k.promoteDeleteErrors} grace_secs=${k.graceSecs}`,
      );
    }
  } catch (e) {
    failures.push(`chunk-gc: ${errorMessage(e)}`);
  }

  if (json) {
    out.failures = failures;
    printJson(out);
  }
  if (failures.length > 0) {
    for (const f of failures.slice(1)) console.error(`engrams: ${f}`);
    fail(failures[0]!);
  }
}
