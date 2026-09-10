import { useHosts } from "./useHosts";
import { useStorageSummary } from "./useStorageSummary";
import { deriveHealthMetrics, operatorIssues, type HealthTone } from "../operator-health";

export type { HealthTone };
export interface OperatorHealth {
  /** null when all-nominal — the rail stays quiet by default. */
  tone: HealthTone | null;
  /** The single worst issue, phrased for a tooltip / screen reader. */
  reason: string | null;
}

// The whole fleet + storage picture distilled to one rail readout: the single
// worst thing an admin should know about without opening the Fleet page. The
// thresholds live in operator-health.ts, shared with the Fleet verdict cell, so
// the rail row and the page can never disagree.
// The readout rides every page for every admin, so it polls at a background
// cadence. At 1 s (the Fleet page rate) it cost ~1 fleet query per second per
// open tab, from the spec editor and the login screen alike.
const TELLTALE_INTERVAL_MS = 30_000;

export function useOperatorHealth(): OperatorHealth {
  const { data: hosts } = useHosts(TELLTALE_INTERVAL_MS);
  const { data: storage } = useStorageSummary(TELLTALE_INTERVAL_MS);

  const h = hosts ?? [];
  if (h.length === 0) return { tone: null, reason: null }; // nothing registered — nothing to watch

  const [top] = operatorIssues(deriveHealthMetrics(h, storage));
  return top ? { tone: top.tone, reason: top.text } : { tone: null, reason: null };
}
