import { useHosts } from './useHosts';
import { useStorageSummary } from './useStorageSummary';
import { deriveHealthMetrics, operatorIssues, type HealthTone } from '../operator-health';

export type { HealthTone };
export interface OperatorHealth {
  /** null when all-nominal — the rail stays quiet by default. */
  tone: HealthTone | null;
  /** The single worst issue, phrased for a tooltip / screen reader. */
  reason: string | null;
}

// The whole fleet + storage picture distilled to one rail telltale: the single
// worst thing an operator should know about without opening the cockpit. The
// thresholds live in operator-health.ts, shared with the Overview verdict, so the
// dot and the cockpit can never disagree.
export function useOperatorHealth(): OperatorHealth {
  const { data: hosts } = useHosts();
  const { data: storage } = useStorageSummary();

  const h = hosts ?? [];
  if (h.length === 0) return { tone: null, reason: null }; // nothing registered — nothing to watch

  const [top] = operatorIssues(deriveHealthMetrics(h, storage));
  return top ? { tone: top.tone, reason: top.text } : { tone: null, reason: null };
}
