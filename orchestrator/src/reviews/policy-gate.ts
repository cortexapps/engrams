/** Deterministic verifier fold and comment-cap policy (ADR 0100). */

import type {
  FindingCounts,
  ReviewDetail,
  ReviewFindingRow,
} from "../db/reviews.ts";

export interface FindingDecision {
  finding: ReviewFindingRow;
  /** `post` is a policy disposition; durable posting changes it to `posted`. */
  state: string;
  /** Effective confidence after the verifier verdict is folded. */
  confidence: string;
  verdictReason: string | null;
}

export interface PolicyDecision {
  toPost: FindingDecision[];
  uiOnly: FindingDecision[];
  suppressed: FindingDecision[];
  counts: FindingCounts;
  overflow: number;
}

const SEVERITY_RANK: Readonly<Record<string, number>> = {
  critical: 4,
  high: 3,
  medium: 2,
  low: 1,
};

const CONFIDENCE_RANK: Readonly<Record<string, number>> = {
  high: 3,
  medium: 2,
  low: 1,
};

function decision(
  finding: ReviewFindingRow,
  state: string,
  confidence = finding.confidence,
  verdictReason = finding.verdictReason,
): FindingDecision {
  return {
    finding: {
      ...finding,
      confidence,
      verdictReason,
    },
    state,
    confidence,
    verdictReason,
  };
}

function addCount(counts: FindingCounts, finding: ReviewFindingRow): void {
  counts.total++;
  switch (finding.severity) {
    case "critical":
      counts.critical++;
      break;
    case "high":
      counts.high++;
      break;
    case "medium":
      counts.medium++;
      break;
    case "low":
      counts.low++;
      break;
  }
}

function normalizedCap(value: number | undefined): number {
  if (value === undefined) return 10;
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.floor(value));
}

/**
 * Fold verifier output into posting dispositions. Enrollment does not yet have
 * category, threshold, or path-filter columns, so the v1 machine config is
 * deliberately limited to the comment cap.
 */
export function runPolicyGate(
  detail: ReviewDetail,
  opts: { commentCap?: number } = {},
): PolicyDecision {
  const verdicts = new Map(detail.verdicts.map((verdict) => [verdict.findingId, verdict]));
  const confirmed: FindingDecision[] = [];
  const uiOnly: FindingDecision[] = [];
  const suppressed: FindingDecision[] = [];
  const passedThrough: FindingDecision[] = [];

  for (const finding of detail.findings) {
    if (finding.state !== "candidate") {
      const existing = decision(finding, finding.state);
      if (finding.state === "posted" || finding.state === "confirmed") {
        passedThrough.push(existing);
      } else if (finding.state === "ui_only") {
        uiOnly.push(existing);
      } else {
        suppressed.push(existing);
      }
      continue;
    }

    const verdict = verdicts.get(finding.id);
    if (verdict?.verdict === "refuted") {
      suppressed.push(
        decision(finding, "suppressed_refuted", verdict.confidence, verdict.reasoning),
      );
    } else if (verdict?.verdict === "confirmed") {
      confirmed.push(decision(finding, "post", verdict.confidence));
    } else {
      // Missing or malformed verifier output is unverified and never posts.
      uiOnly.push(decision(finding, "ui_only", "low"));
    }
  }

  confirmed.sort((left, right) => {
    const severity = (SEVERITY_RANK[right.finding.severity] ?? 0)
      - (SEVERITY_RANK[left.finding.severity] ?? 0);
    if (severity !== 0) return severity;
    return (CONFIDENCE_RANK[right.confidence] ?? 0)
      - (CONFIDENCE_RANK[left.confidence] ?? 0);
  });

  const cap = normalizedCap(opts.commentCap);
  const toPost = [...passedThrough, ...confirmed.slice(0, cap)];
  const overflowDecisions = confirmed
    .slice(cap)
    .map((item) => decision(item.finding, "ui_only", item.confidence, item.verdictReason));
  uiOnly.push(...overflowDecisions);

  const counts: FindingCounts = {
    critical: 0,
    high: 0,
    medium: 0,
    low: 0,
    total: 0,
  };
  for (const item of [...toPost, ...uiOnly]) addCount(counts, item.finding);

  return {
    toPost,
    uiOnly,
    suppressed,
    counts,
    overflow: overflowDecisions.length,
  };
}
