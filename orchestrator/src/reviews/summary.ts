/** The markdown a review posts on a pull request: the summary and each
 * inline comment body. */

import type { ReviewFindingRow } from "../db/reviews.ts";
import type { FindingDecision, PolicyDecision } from "./policy-gate.ts";

export interface ReviewSummaryInput {
  reviewId: string;
  reviewUrl: string;
  decision: PolicyDecision;
  /** False means every non-suppressed finding must be preserved in the body. */
  inlinePosted: boolean;
}

const CATEGORY_LABELS: Readonly<Record<string, string>> = {
  "security-privacy": "🔒 Security & Privacy",
  "stability-availability": "🩺 Stability & Availability",
  "data-integrity-integration": "🗄️ Data Integrity & Integration",
  "functional-correctness": "🎯 Functional Correctness",
  "performance-scalability": "🚀 Performance & Scalability",
  "maintainability-quality": "📐 Maintainability & Code Quality",
};

function findingLocation(item: FindingDecision): string {
  const line = item.finding.endLine ?? item.finding.startLine;
  return line == null ? item.finding.path : `${item.finding.path}:L${line}`;
}

function quoteFinding(item: FindingDecision): string {
  const body = item.finding.bodyMd
    .split("\n")
    .map((line) => `> ${line}`)
    .join("\n");
  return [
    `> \`${findingLocation(item)}\` — **${item.finding.title}**`,
    ">",
    body,
  ].join("\n");
}

/** The review summary. The marker is the last line: the post action scans
 * for it to find a review it already posted. */
export function buildReviewSummary(input: ReviewSummaryInput): string {
  const visible = [...input.decision.toPost, ...input.decision.uiOnly];
  const categoryCounts = new Map<string, number>();
  for (const item of visible) {
    categoryCounts.set(
      item.finding.category,
      (categoryCounts.get(item.finding.category) ?? 0) + 1,
    );
  }

  const verdict = input.decision.counts.total === 0
    ? "No findings."
    : input.inlinePosted
    ? `${input.decision.toPost.length} finding${input.decision.toPost.length === 1 ? "" : "s"} posted inline.`
    : `${input.decision.counts.total} finding${input.decision.counts.total === 1 ? "" : "s"} included in this summary.`;
  const severities = input.decision.counts;
  const categories = [...categoryCounts.entries()]
    .map(([category, count]) => `${CATEGORY_LABELS[category] ?? category}: ${count}`)
    .join(" · ");
  const details = input.inlinePosted ? input.decision.uiOnly : visible;

  const lines = [
    "## Engrams review",
    "",
    `**Verdict:** ${verdict}`,
    `**Severity:** Critical ${severities.critical} · High ${severities.high} · Medium ${severities.medium} · Low ${severities.low}`,
    ...(categories ? [`**Categories:** ${categories}`] : []),
    "",
    `[View the full engrams review](${input.reviewUrl})`,
    ...(input.decision.overflow > 0
      ? [
          "",
          `${input.decision.overflow} more finding${input.decision.overflow === 1 ? "" : "s"} on the review page.`,
        ]
      : []),
    ...(details.length > 0
      ? [
          "",
          "### Findings on the review page",
          "",
          details.map(quoteFinding).join("\n\n"),
        ]
      : []),
    "",
    `<!-- engrams-review:${input.reviewId} -->`,
  ];
  return lines.join("\n");
}

export function buildInlineCommentBody(finding: ReviewFindingRow): string {
  const category = CATEGORY_LABELS[finding.category] ?? finding.category;
  return [
    `**${category} · ${finding.severity.toUpperCase()} — ${finding.title}**`,
    "",
    finding.bodyMd,
    ...(finding.suggestedFix
      ? ["", "```suggestion", finding.suggestedFix, "```"]
      : []),
  ].join("\n");
}
