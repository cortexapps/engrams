import type { Review, ReviewFinding, ReviewVerdict } from "../../gen/engram/app/v1/review_pb";
import { SEVERITY_ORDER } from "./review-format";

/**
 * Findings grouped by OUTCOME rather than severity, because the first question a
 * reader has is not "how bad" but "did this actually reach the PR, and if not,
 * why not" (ADR 0100).
 *
 * `ui_only` is one stored state meaning four different things, and the reason is
 * not persisted — it is derived here from rows the client already has:
 *
 *   no verdict row                        → unverified (never posts; the
 *                                            verifier's silence is a low-confidence
 *                                            verdict, not an oversight)
 *   confirmed, no line anchor             → nowhere to hang an inline comment
 *   confirmed, anchored, but not posted   → over the 10-comment cap
 *   refuted                               → the verifier killed it
 *
 * The GitHub 422 batch-fallback (a single stale anchor demotes the whole batch)
 * deliberately folds into "over the cap": it is rare, it demotes everything at
 * once, and separating it would cost a schema column for a distinction no reader
 * acts on differently.
 */
export type Outcome = "posted" | "unverified" | "no_anchor" | "over_cap" | "refuted";

export interface JudgedFinding {
  finding: ReviewFinding;
  /** The verifier's ruling, absent when it never judged this candidate. */
  verdict: ReviewVerdict | undefined;
  outcome: Outcome;
}

export interface OutcomeGroup {
  outcome: Outcome;
  items: JudgedFinding[];
}

/** True when the finding has a line to hang an inline comment on. */
function isAnchored(finding: ReviewFinding): boolean {
  return finding.endLine != null || finding.startLine != null;
}

function outcomeOf(finding: ReviewFinding, verdict: ReviewVerdict | undefined): Outcome {
  if (finding.state === "posted") return "posted";
  if (verdict?.verdict === "refuted" || finding.state === "suppressed_refuted") {
    return "refuted";
  }
  if (!verdict) return "unverified";
  if (!isAnchored(finding)) return "no_anchor";
  return "over_cap";
}

/**
 * Severity first, then the finder's own confidence — the same order the policy
 * gate uses to decide what fits under the comment cap, so what a reader sees
 * first is what the gate considered first.
 */
const CONFIDENCE_ORDER = ["high", "medium", "low"];

function bySeverityThenConfidence(a: JudgedFinding, b: JudgedFinding): number {
  const severity =
    SEVERITY_ORDER.indexOf(a.finding.severity as never) -
    SEVERITY_ORDER.indexOf(b.finding.severity as never);
  if (severity !== 0) return severity;
  return (
    CONFIDENCE_ORDER.indexOf(a.finding.confidence) - CONFIDENCE_ORDER.indexOf(b.finding.confidence)
  );
}

/** Reading order: what shipped, then what didn't and why, then what was killed. */
const OUTCOME_ORDER: Outcome[] = ["posted", "unverified", "no_anchor", "over_cap", "refuted"];

export function judgeFindings(
  findings: readonly ReviewFinding[],
  verdicts: readonly ReviewVerdict[],
): JudgedFinding[] {
  const byFinding = new Map(verdicts.map((v) => [v.findingId, v]));
  return findings.map((finding) => {
    const verdict = byFinding.get(finding.id);
    return { finding, verdict, outcome: outcomeOf(finding, verdict) };
  });
}

/** Non-empty outcome groups, in reading order, each internally ranked. */
export function groupByOutcome(judged: readonly JudgedFinding[]): OutcomeGroup[] {
  const groups = new Map<Outcome, JudgedFinding[]>();
  for (const item of judged) {
    const existing = groups.get(item.outcome);
    if (existing) existing.push(item);
    else groups.set(item.outcome, [item]);
  }
  return OUTCOME_ORDER.filter((outcome) => groups.has(outcome)).map((outcome) => ({
    outcome,
    items: groups.get(outcome)!.sort(bySeverityThenConfidence),
  }));
}

/**
 * The trust signal: how much of what the finder reported survived the verifier.
 * `kept` counts everything still standing — posted or visible here only — which
 * is the honest denominator for "should I believe this pass".
 */
export interface KeptRatio {
  kept: number;
  total: number;
  refuted: number;
  posted: number;
}

export function keptRatio(judged: readonly JudgedFinding[]): KeptRatio {
  let refuted = 0;
  let posted = 0;
  for (const item of judged) {
    if (item.outcome === "refuted") refuted++;
    if (item.outcome === "posted") posted++;
  }
  return { kept: judged.length - refuted, total: judged.length, refuted, posted };
}

/**
 * A pass that never reached posting leaves every finding at `candidate` — only
 * `postReviewResults` advances them. So a failed or still-running pass shows
 * candidates, and calling those "not posted, over the cap" would be a lie about
 * why. Callers use this to say "not judged yet" instead.
 */
export function isUnresolvedPass(review: Review): boolean {
  return review.status !== "posted";
}
