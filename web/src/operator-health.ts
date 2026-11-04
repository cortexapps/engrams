import type { HostView } from "./lib/types";

// The single source of truth for what counts as a fleet problem and how
// serious it is. Both surfaces that judge platform health — the Fleet page's
// verdict cell (pages/Fleet.tsx) and the Settings rail's Fleet row
// (hooks/useOperatorHealth.ts) — read from here, so the dot and the page can
// never disagree about whether something is wrong.

export type HealthTone = "caution" | "critical";
export type HealthIssueKind = "offline" | "capacity" | "draining" | "caps";
export interface HealthIssue {
  kind: HealthIssueKind;
  tone: HealthTone;
  /** Short phrase for a verdict headline, tooltip, or screen reader. */
  text: string;
}

export interface HealthMetrics {
  dead: number;
  draining: number;
  /** Fleet capacity used, 0–100; 0 when nothing is registered. */
  capPct: number;
  /** ADR 0068: hosts reporting at least one failing capability
   *  (`Failed`/`Unknown` on a probed vector) — the fleet-view surface
   *  for what used to be the silent "no capacity with free hosts"
   *  mystery mode. */
  capsFailing: number;
}

export function deriveHealthMetrics(hosts: HostView[]): HealthMetrics {
  const dead = hosts.filter((x) => x.status === "dead").length;
  const draining = hosts.filter((x) => x.status === "draining").length;
  const used = hosts.reduce((a, x) => a + x.capacity_used_mib, 0);
  const tot = hosts.reduce((a, x) => a + x.capacity_total_mib, 0);
  const capPct = tot > 0 ? Math.round((used / tot) * 100) : 0;

  const capsFailing = hosts.filter((x) => x.failing_capabilities.length > 0).length;

  return { dead, draining, capPct, capsFailing };
}

// Worst-first: critical issues precede caution ones, and within a tier the push
// order encodes priority (Array.prototype.sort is stable). So `issues[0]` is
// always the one thing an operator should look at first.
export function operatorIssues(m: HealthMetrics): HealthIssue[] {
  const issues: (HealthIssue & { sev: 2 | 3 })[] = [];
  // Critical — needs eyes now.
  if (m.dead > 0)
    issues.push({
      kind: "offline",
      sev: 3,
      tone: "critical",
      text: `${m.dead} host${m.dead > 1 ? "s" : ""} offline`,
    });
  if (m.capPct >= 90)
    issues.push({
      kind: "capacity",
      sev: 3,
      tone: "critical",
      text: `fleet at ${m.capPct}% capacity`,
    });
  // Caution — worth a glance.
  if (m.draining > 0)
    issues.push({
      kind: "draining",
      sev: 2,
      tone: "caution",
      text: `${m.draining} host${m.draining > 1 ? "s" : ""} draining`,
    });
  if (m.capPct >= 70 && m.capPct < 90)
    issues.push({
      kind: "capacity",
      sev: 2,
      tone: "caution",
      text: `fleet at ${m.capPct}% capacity`,
    });
  if (m.capsFailing > 0)
    issues.push({
      kind: "caps",
      sev: 2,
      tone: "caution",
      text: `${m.capsFailing} host${m.capsFailing > 1 ? "s" : ""} failing capability checks`,
    });

  issues.sort((a, b) => b.sev - a.sev);
  return issues.map(({ kind, tone, text }) => ({ kind, tone, text }));
}
