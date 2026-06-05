import type { HostView, StorageSummaryResponse } from './types';
import { secondsSince } from './format';

// The single source of truth for what counts as a fleet/storage problem and how
// serious it is. Both surfaces that judge platform health — the Operator cockpit
// verdict (pages/operator/Overview.tsx) and the rail telltale
// (hooks/useOperatorHealth.ts) — read from here, so the dot and the dashboard can
// never disagree about whether something is wrong.

export type HealthTone = 'caution' | 'critical';
export interface HealthIssue {
  tone: HealthTone;
  /** Short phrase for a verdict headline, tooltip, or screen reader. */
  text: string;
}

const RPO_WINDOW_S = 60; // a chunk-tracked sandbox past this since last flush is "stale"

export interface HealthMetrics {
  dead: number;
  draining: number;
  /** Fleet capacity used, 0–100; 0 when nothing is registered. */
  capPct: number;
  /** Average base locality 0–100, or null when nothing is chunk-tracked yet. */
  locality: number | null;
  rpoStale: number;
}

export function deriveHealthMetrics(
  hosts: HostView[],
  storage: StorageSummaryResponse | undefined,
): HealthMetrics {
  const dead = hosts.filter((x) => x.status === 'dead').length;
  const draining = hosts.filter((x) => x.status === 'draining').length;
  const used = hosts.reduce((a, x) => a + x.capacity_used_mib, 0);
  const tot = hosts.reduce((a, x) => a + x.capacity_total_mib, 0);
  const capPct = tot > 0 ? Math.round((used / tot) * 100) : 0;

  const hasStorage = storage != null && storage.tracked_sandboxes > 0;
  const locality = hasStorage ? storage.avg_locality_pct : null;
  const rpoStale = (storage?.rows ?? []).filter((r) => secondsSince(r.last_flush_at) > RPO_WINDOW_S).length;

  return { dead, draining, capPct, locality, rpoStale };
}

// Worst-first: critical issues precede caution ones, and within a tier the push
// order encodes priority (Array.prototype.sort is stable). So `issues[0]` is
// always the one thing an operator should look at first.
export function operatorIssues(m: HealthMetrics): HealthIssue[] {
  const issues: { sev: 2 | 3; tone: HealthTone; text: string }[] = [];
  // Critical — needs eyes now.
  if (m.dead > 0) issues.push({ sev: 3, tone: 'critical', text: `${m.dead} host${m.dead > 1 ? 's' : ''} offline` });
  if (m.capPct >= 90) issues.push({ sev: 3, tone: 'critical', text: `fleet at ${m.capPct}% capacity` });
  if (m.locality != null && m.locality < 50) issues.push({ sev: 3, tone: 'critical', text: `base locality ${m.locality}%` });
  // Caution — worth a glance.
  if (m.draining > 0) issues.push({ sev: 2, tone: 'caution', text: `${m.draining} host${m.draining > 1 ? 's' : ''} draining` });
  if (m.capPct >= 70 && m.capPct < 90) issues.push({ sev: 2, tone: 'caution', text: `fleet at ${m.capPct}% capacity` });
  if (m.locality != null && m.locality >= 50 && m.locality < 80) issues.push({ sev: 2, tone: 'caution', text: `base locality ${m.locality}%` });
  if (m.rpoStale > 0) issues.push({ sev: 2, tone: 'caution', text: `${m.rpoStale} sandbox${m.rpoStale > 1 ? 'es' : ''} past flush window` });

  issues.sort((a, b) => b.sev - a.sev);
  return issues.map(({ tone, text }) => ({ tone, text }));
}
