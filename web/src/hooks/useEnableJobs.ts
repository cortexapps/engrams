import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import {
  listEnableJobs,
  retryEnableJob,
  listEnabledImages,
} from "../gen/engram/app/v1/image-ImageService_connectquery";
import type { EnableJob } from "../lib/types";
import type { EnableJob as ProtoEnableJob } from "../gen/engram/app/v1/image_pb";

// ADR 0036: enabling an image is asynchronous. POST returns 202 with
// an EnableJob; the coordinator's scanner drives the pipeline
// (pending → materializing → capturing → prestaging → ready | failed,
// "prestaging" added by the ADR 0036 amendment / issue #538), updating
// `chunks_done/chunks_total` as chunks materialize. This hook polls
// the job list while anything is in flight — REAL progress from the
// server, replacing the old wall-clock-driven stage guesser.

export const ENABLE_JOBS_KEY = createConnectQueryKey({
  schema: listEnableJobs,
  input: {},
  cardinality: "finite",
});

function protoEnableJobToLegacy(j: ProtoEnableJob): EnableJob {
  return {
    id: j.id,
    image_uri: j.imageUri,
    manifest_digest: j.manifestDigest ?? null,
    state: j.state as EnableJob["state"],
    chunks_total: j.chunksTotal ?? null,
    chunks_done: j.chunksDone,
    attempts: j.attempts,
    error: j.error ?? null,
    created_at: j.createdAt,
    updated_at: j.updatedAt,
    capture_phase: j.capturePhase ?? null,
    warm_stage: j.warmStage ?? null,
    warm_stage_started_at: j.warmStageStartedAt ?? null,
    output_tail: j.outputTail ?? null,
    prestage_hosts: j.prestageHosts,
  };
}

export function isJobActive(job: EnableJob): boolean {
  return job.state !== "ready" && job.state !== "failed";
}

/** Recent enable jobs (newest first), polled at 2s while any job is
 * active and parked otherwise. Surviving a page reload is the point:
 * an in-flight enable shows up here without any client-held state. */
export function useEnableJobs() {
  const qc = useQueryClient();
  const query = useQuery(
    listEnableJobs,
    {},
    {
      select: (data) => data.jobs.map(protoEnableJobToLegacy),
      refetchOnWindowFocus: true,
      // Poll only while something is moving. 2s matches the
      // coordinator-side checkpoint cadence — faster would just
      // re-read the same counters.
      refetchInterval: (q) => {
        // q.state.data may be the raw ListEnableJobsResponse (with .jobs array)
        // or the selected EnableJob[] depending on tanstack query internals.
        // Handle both shapes safely.
        const raw = q.state.data;
        if (!raw) return false;
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const jobs: EnableJob[] = Array.isArray(raw) ? raw : ((raw as any).jobs ?? []);
        return jobs.some(isJobActive) ? 2000 : false;
      },
    },
  );

  // When a job leaves the active set (reached ready/failed), the
  // enabled-images list almost certainly changed — refresh it so the
  // new row appears without a manual reload.
  const prevActive = useRef<Set<string>>(new Set());
  useEffect(() => {
    const jobs = query.data ?? [];
    const active = new Set(jobs.filter(isJobActive).map((j) => j.id));
    for (const id of prevActive.current) {
      if (!active.has(id)) {
        qc.invalidateQueries({
          queryKey: createConnectQueryKey({
            schema: listEnabledImages,
            input: {},
            cardinality: "finite",
          }),
        });
        break;
      }
    }
    prevActive.current = active;
  }, [query.data, qc]);

  return query;
}

/** Mutation: re-queue a `failed` enable job (admin). */
export function useRetryEnableJob() {
  const qc = useQueryClient();
  return useMutation(retryEnableJob, {
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ENABLE_JOBS_KEY });
    },
  });
}
