import { useEffect, useState } from 'react';

// Stage labels for the multi-second `POST /api/enabled-images` wait.
// The backend is monolithically synchronous (it pulls the OCI
// artifact, slices the chunks blob into BlobStorage, then materializes
// the canonical snapshot before returning), but the user staring at a
// spinner needs to know *something* is moving. We don't have real
// progress events from the server — instead we drive a stage marker
// off elapsed time, with thresholds calibrated against the observed
// wall-clock on a typical 350 MB demo image. Each stage's copy is
// intentionally a present-tense verb so it reads like a thing
// happening, not a thing planned.
const STAGES: { at_ms: number; label: string }[] = [
  { at_ms: 0, label: 'fetching manifest' },
  { at_ms: 1500, label: 'reading bundle' },
  { at_ms: 3000, label: 'rehydrating chunks into blob storage' },
  { at_ms: 9000, label: 'reconstructing canonical snapshot' },
  { at_ms: 15000, label: 'prefetching base layers on every host' },
  { at_ms: 25000, label: 'almost there' },
];

/** Returns the current stage label + the elapsed milliseconds
 * since the submit started. Mount during an in-flight enable
 * (gate on `isPending`); when not pending, returns `null`. */
export function useEnableProgress(isPending: boolean): {
  label: string;
  elapsedMs: number;
} | null {
  const [elapsedMs, setElapsedMs] = useState(0);

  useEffect(() => {
    if (!isPending) {
      setElapsedMs(0);
      return;
    }
    const start = performance.now();
    const id = window.setInterval(() => {
      setElapsedMs(performance.now() - start);
    }, 200);
    return () => window.clearInterval(id);
  }, [isPending]);

  if (!isPending) return null;
  // Pick the latest stage we've crossed. Reverse iter so the
  // first match is the most recent.
  const stage = STAGES.slice().reverse().find((s) => elapsedMs >= s.at_ms);
  return { label: stage?.label ?? STAGES[0].label, elapsedMs };
}
