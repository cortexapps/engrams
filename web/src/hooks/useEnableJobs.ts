import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { fetchEnableJobs, retryEnableJob } from "../api";
import type { EnableJob } from "../types";

// ADR 0036: enabling an image is asynchronous. POST returns 202 with
// an EnableJob; the coordinator's scanner drives the pipeline
// (pending → materializing → capturing → ready | failed), updating
// `chunks_done/chunks_total` as chunks materialize. This hook polls
// the job list while anything is in flight — REAL progress from the
// server, replacing the old wall-clock-driven stage guesser.

export const ENABLE_JOBS_KEY = ["enable-jobs"] as const;

export function isJobActive(job: EnableJob): boolean {
  return job.state !== "ready" && job.state !== "failed";
}

/** Recent enable jobs (newest first), polled at 2s while any job is
 * active and parked otherwise. Surviving a page reload is the point:
 * an in-flight enable shows up here without any client-held state. */
export function useEnableJobs() {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: ENABLE_JOBS_KEY,
    queryFn: fetchEnableJobs,
    refetchOnWindowFocus: true,
    // Poll only while something is moving. 2s matches the
    // coordinator-side checkpoint cadence — faster would just
    // re-read the same counters.
    refetchInterval: (q) => {
      const jobs = q.state.data as EnableJob[] | undefined;
      return jobs?.some(isJobActive) ? 2000 : false;
    },
  });

  // When a job leaves the active set (reached ready/failed), the
  // enabled-images list almost certainly changed — refresh it so the
  // new row appears without a manual reload.
  const prevActive = useRef<Set<string>>(new Set());
  useEffect(() => {
    const jobs = query.data ?? [];
    const active = new Set(jobs.filter(isJobActive).map((j) => j.id));
    for (const id of prevActive.current) {
      if (!active.has(id)) {
        qc.invalidateQueries({ queryKey: ["enabled-images"] });
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
  return useMutation({
    mutationFn: (jobId: string) => retryEnableJob(jobId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ENABLE_JOBS_KEY });
    },
  });
}
