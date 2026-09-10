import { useMutation } from "@connectrpc/connect-query";

import { chunkGc } from "../gen/engram/app/v1/fleet-FleetService_connectquery";

/** Chunk garbage collection in DRY-RUN mode: walks the chunk space and
 * reports what a real sweep would mark, deleting nothing. The Storage page's
 * one action — an operator's answer to "how much is GC about to reclaim?"
 * without touching the store. The live sweep stays a scheduled job. */
export function useChunkGcDryRun() {
  return useMutation(chunkGc);
}
