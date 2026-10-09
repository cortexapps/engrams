/** Profile-routing benchmark: score a `decide` router against historical
 * routing decisions (which profile a request actually ran on).
 *
 * The samples are exported from a deployment's own database by its operator
 * and never enter the repository. This module is the pure half — the request
 * shape (the same questions the Slack relay asks) and the metrics — so the
 * script in scripts/bench-profile-routing.ts stays a thin I/O loop.
 */

import type { ProfileCard } from "./profile-cards.ts";

/** One historical routing decision. `label` is the profile id it ran on. */
export interface RoutingSample {
  slice: string;
  id: string;
  created_at: string;
  label: string;
  /** Who started it (a per-user prior baseline). */
  user?: string | null;
  /** Slack channel (a per-channel prior baseline). */
  channel?: string | null;
  /** The request: a full prompt, a mention text, or a truncated title. */
  prompt?: string | null;
  text?: string | null;
  title?: string | null;
  /** A one-line summary of the request, when the request itself is truncated. */
  summary?: string | null;
}

/** The decide `state` for a sample, or null when the sample carries no
 * request text. Slack user mentions (`<@U123>`) are noise to the router. */
export function sampleState(sample: RoutingSample): Record<string, string> | null {
  const clean = (s: string | null | undefined) => (s ?? "").replace(/<@[A-Z0-9]+>/g, "").trim();
  const message = clean(sample.prompt) || clean(sample.text) || clean(sample.title);
  const gist = clean(sample.summary);
  if (!message && !gist) return null;
  return { ...(message ? { message } : {}), ...(gist ? { gist } : {}) };
}

export interface RoutingResult {
  sample: RoutingSample;
  /** Absent when the call failed. */
  predicted?: string;
  ranked?: string[];
  confidence?: number;
  wantsChoice?: number;
  latencyMs: number;
  cost?: number | null;
  error?: string;
}

export interface SliceReport {
  slice: string;
  n: number;
  errors: number;
  top1: number;
  top2: number;
  /** For each threshold: the share of samples routed (confidence >= t) and
   * the accuracy on those. The rest would be asked. */
  coverage: Array<{ threshold: number; coverage: number; accuracy: number | null }>;
  /** Confidence bins: mean confidence vs observed accuracy. */
  calibration: Array<{ bin: string; n: number; meanConfidence: number; accuracy: number }>;
  /** label → predicted → count. */
  confusion: Record<string, Record<string, number>>;
  wantsChoiceRate: number;
  baselines: { majority: number; userPrior: number | null; channelPrior: number | null };
  latencyMs: { p50: number; p95: number; max: number };
  cost: { total: number; per1k: number };
}

export const COVERAGE_THRESHOLDS = [0, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const i = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[i]!;
}

function mostFrequent(counts: Map<string, number>): string | undefined {
  let best: string | undefined;
  let bestN = -1;
  for (const [k, n] of counts) {
    if (n > bestN) {
      best = k;
      bestN = n;
    }
  }
  return best;
}

/** Accuracy of "the profile this key used most so far" — a chronological
 * prior that only sees EARLIER samples (all slices). Samples with no history
 * for the key fall back to the global majority so far. Null when no sample in
 * the slice has the key. */
function priorAccuracy(
  slice: RoutingSample[],
  all: RoutingSample[],
  key: (s: RoutingSample) => string | null | undefined,
): number | null {
  const scored = slice.filter((s) => key(s));
  if (scored.length === 0) return null;
  const ordered = [...all].sort((a, b) => a.created_at.localeCompare(b.created_at));
  const byKey = new Map<string, Map<string, number>>();
  const global = new Map<string, number>();
  const prediction = new Map<string, string | undefined>();
  for (const s of ordered) {
    const k = key(s);
    const counts = k ? byKey.get(k) : undefined;
    prediction.set(s.id, (counts && mostFrequent(counts)) ?? mostFrequent(global));
    if (k) {
      const c = byKey.get(k) ?? new Map<string, number>();
      c.set(s.label, (c.get(s.label) ?? 0) + 1);
      byKey.set(k, c);
    }
    global.set(s.label, (global.get(s.label) ?? 0) + 1);
  }
  return scored.filter((s) => prediction.get(s.id) === s.label).length / scored.length;
}

/** Score one slice. `all` is every sample (all slices), for the priors. */
export function scoreSlice(slice: string, results: RoutingResult[], all: RoutingSample[]): SliceReport {
  const ok = results.filter((r) => r.predicted !== undefined);
  const correct = (r: RoutingResult) => r.predicted === r.sample.label;
  const n = results.length;

  const coverage = COVERAGE_THRESHOLDS.map((threshold) => {
    const routed = ok.filter((r) => (r.confidence ?? 0) >= threshold);
    return {
      threshold,
      coverage: n === 0 ? 0 : routed.length / n,
      accuracy: routed.length === 0 ? null : routed.filter(correct).length / routed.length,
    };
  });

  const bins = [0, 0.2, 0.4, 0.6, 0.8, 1.0001];
  const calibration = bins.slice(0, -1).flatMap((lo, i) => {
    const hi = bins[i + 1]!;
    const inBin = ok.filter((r) => (r.confidence ?? 0) >= lo && (r.confidence ?? 0) < hi);
    if (inBin.length === 0) return [];
    return [
      {
        bin: `${lo.toFixed(1)}-${Math.min(hi, 1).toFixed(1)}`,
        n: inBin.length,
        meanConfidence: inBin.reduce((s, r) => s + (r.confidence ?? 0), 0) / inBin.length,
        accuracy: inBin.filter(correct).length / inBin.length,
      },
    ];
  });

  const confusion: Record<string, Record<string, number>> = {};
  for (const r of ok) {
    const row = (confusion[r.sample.label] ??= {});
    row[r.predicted!] = (row[r.predicted!] ?? 0) + 1;
  }

  const labels = new Map<string, number>();
  for (const r of results) labels.set(r.sample.label, (labels.get(r.sample.label) ?? 0) + 1);
  const majority = n === 0 ? 0 : Math.max(0, ...labels.values()) / n;
  const samples = results.map((r) => r.sample);

  const latencies = ok.map((r) => r.latencyMs).sort((a, b) => a - b);
  const totalCost = ok.reduce((s, r) => s + (r.cost ?? 0), 0);
  return {
    slice,
    n,
    errors: n - ok.length,
    top1: n === 0 ? 0 : ok.filter(correct).length / n,
    top2: n === 0 ? 0 : ok.filter((r) => (r.ranked ?? []).slice(0, 2).includes(r.sample.label)).length / n,
    coverage,
    calibration,
    confusion,
    wantsChoiceRate: ok.length === 0 ? 0 : ok.filter((r) => (r.wantsChoice ?? 0) >= 0.5).length / ok.length,
    baselines: {
      majority,
      userPrior: priorAccuracy(samples, all, (s) => s.user),
      channelPrior: priorAccuracy(samples, all, (s) => s.channel),
    },
    latencyMs: {
      p50: percentile(latencies, 50),
      p95: percentile(latencies, 95),
      max: latencies.at(-1) ?? 0,
    },
    cost: { total: totalCost, per1k: ok.length === 0 ? 0 : (totalCost / ok.length) * 1000 },
  };
}

/** Samples whose label is not a current profile cannot be scored. */
export function scorableSamples(samples: RoutingSample[], cards: ProfileCard[]): RoutingSample[] {
  const ids = new Set(cards.map((c) => c.id));
  return samples.filter((s) => ids.has(s.label) && sampleState(s) !== null);
}
