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
 *   confirmed, anchored, gate ran         → over the 10-comment cap
 *   confirmed, anchored, gate never ran   → unresolved; the pass died or is still
 *                                            going, so no decision was made
 *   refuted                               → the verifier killed it
 *
 * The GitHub 422 batch-fallback (a single stale anchor demotes the whole batch)
 * deliberately folds into "over the cap": it is rare, it demotes everything at
 * once, and separating it would cost a schema column for a distinction no reader
 * acts on differently.
 */
export type Outcome = "posted" | "unresolved" | "unverified" | "no_anchor" | "refuted" | "over_cap";

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

function outcomeOf(
  finding: ReviewFinding,
  verdict: ReviewVerdict | undefined,
  /** False while the pass is still running or after it died — see below. */
  gateRan: boolean,
): Outcome {
  if (finding.state === "posted") return "posted";
  if (verdict?.verdict === "refuted" || finding.state === "suppressed_refuted") {
    return "refuted";
  }
  if (!verdict) return "unverified";
  if (!isAnchored(finding)) return "no_anchor";
  // Only `postReviewResults` advances a finding past `candidate`, so on a pass
  // that never reached posting EVERY confirmed anchored finding still looks
  // exactly like an over-cap one. Calling it "over the comment cap" would be a
  // statement about a gate that never ran — and the page would simultaneously say
  // the pass didn't finish and that this finding lost a race inside it.
  return gateRan ? "over_cap" : "unresolved";
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
const OUTCOME_ORDER: Outcome[] = [
  "posted",
  "unresolved",
  "unverified",
  "no_anchor",
  "over_cap",
  "refuted",
];

/**
 * The posting gate is the only thing that advances a finding past `candidate`, and
 * it runs once, at the end. So "did the gate run?" is exactly "did this pass reach
 * `posted`?" — and every outcome that reasons about the gate's *decision* is only
 * meaningful once it has.
 */
export function gateHasRun(review: Review): boolean {
  return review.status === "posted";
}

export function judgeFindings(
  findings: readonly ReviewFinding[],
  verdicts: readonly ReviewVerdict[],
  review: Review,
): JudgedFinding[] {
  const byFinding = new Map(verdicts.map((v) => [v.findingId, v]));
  const gateRan = gateHasRun(review);
  return findings.map((finding) => {
    const verdict = byFinding.get(finding.id);
    return { finding, verdict, outcome: outcomeOf(finding, verdict, gateRan) };
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
