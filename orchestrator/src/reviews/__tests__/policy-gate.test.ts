import { describe, expect, test } from "bun:test";

import type {
  ReviewDetail,
  ReviewFindingRow,
  ReviewRow,
  ReviewVerdictRow,
} from "../../db/reviews.ts";
import { runPolicyGate } from "../policy-gate.ts";

const review: ReviewRow = {
  id: "review-1",
  repo: "openai/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "head",
  baseSha: "base",
  trigger: "opened",
  status: "verifying",
  githubReviewId: null,
  statusCommentId: null,
  finderSessionId: null,
  verifierSessionId: null,
  summaryMd: null,
  prTitle: null,
  prAuthor: null,
  headBranch: null,
  baseBranch: null,
  prState: null,
  additions: null,
  deletions: null,
  changedFiles: null,
  createdAt: new Date(0),
  updatedAt: new Date(0),
};

function finding(
  id: string,
  overrides: Partial<ReviewFindingRow> = {},
): ReviewFindingRow {
  return {
    id,
    reviewId: review.id,
    path: `src/${id}.ts`,
    startLine: 10,
    endLine: 12,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "medium",
    confidence: "medium",
    title: `Finding ${id}`,
    bodyMd: "The behavior is incorrect.",
    suggestedFix: null,
    evidence: [`src/${id}.ts`],
    state: "candidate",
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: `call-${id}`,
    createdAt: new Date(0),
    ...overrides,
  };
}

function verdict(
  findingId: string,
  result: "confirmed" | "refuted",
  confidence: string,
): ReviewVerdictRow {
  return {
    id: `verdict-${findingId}`,
    findingId,
    verdict: result,
    confidence,
    reasoning: `${result} because the relevant branch was traced.`,
    sessionId: "verifier-session",
    toolCallId: `verdict-call-${findingId}`,
    createdAt: new Date(1),
  };
}

function detail(
  findings: ReviewFindingRow[],
  verdicts: ReviewVerdictRow[] = [],
): ReviewDetail {
  return { review, findings, verdicts };
}

describe("runPolicyGate", () => {
  test("suppresses refuted findings and keeps verifier reasoning", () => {
    const candidate = finding("refuted", { confidence: "high" });
    const result = runPolicyGate(detail([
      candidate,
    ], [verdict(candidate.id, "refuted", "medium")]));

    expect(result.toPost).toEqual([]);
    expect(result.uiOnly).toEqual([]);
    expect(result.suppressed[0]).toMatchObject({
      state: "suppressed_refuted",
      confidence: "medium",
      verdictReason: "refuted because the relevant branch was traced.",
    });
  });

  test("posts confirmed findings with verifier confidence", () => {
    const candidate = finding("confirmed", { confidence: "low" });
    const result = runPolicyGate(detail([
      candidate,
    ], [verdict(candidate.id, "confirmed", "high")]));

    expect(result.toPost[0]).toMatchObject({
      state: "post",
      confidence: "high",
      finding: { id: candidate.id, confidence: "high" },
    });
  });

  test("keeps a candidate without a verdict UI-only at low confidence", () => {
    const result = runPolicyGate(detail([
      finding("unverified", { confidence: "high" }),
    ]));

    expect(result.toPost).toEqual([]);
    expect(result.uiOnly[0]).toMatchObject({
      state: "ui_only",
      confidence: "low",
      finding: { confidence: "low" },
    });
  });

  test("caps confirmed findings after ranking severity then confidence", () => {
    const findings = [
      finding("medium-high", { severity: "medium", confidence: "low" }),
      finding("critical-low", { severity: "critical", confidence: "high" }),
      finding("high-medium", { severity: "high", confidence: "high" }),
      finding("high-high", { severity: "high", confidence: "low" }),
    ];
    const verdicts = [
      verdict("medium-high", "confirmed", "high"),
      verdict("critical-low", "confirmed", "low"),
      verdict("high-medium", "confirmed", "medium"),
      verdict("high-high", "confirmed", "high"),
    ];

    const result = runPolicyGate(detail(findings, verdicts), { commentCap: 2 });

    expect(result.toPost.map((item) => item.finding.id)).toEqual([
      "critical-low",
      "high-high",
    ]);
    expect(result.uiOnly.map((item) => item.finding.id)).toEqual([
      "high-medium",
      "medium-high",
    ]);
    expect(result.uiOnly.every((item) => item.state === "ui_only")).toBe(true);
    expect(result.overflow).toBe(2);
  });

  test("counts every non-suppressed finding by severity", () => {
    const findings = [
      finding("critical", { severity: "critical" }),
      finding("high", { severity: "high" }),
      finding("medium", { severity: "medium" }),
      finding("low", { severity: "low" }),
      finding("refuted", { severity: "critical" }),
    ];
    const verdicts = [
      verdict("critical", "confirmed", "high"),
      verdict("high", "confirmed", "high"),
      verdict("medium", "confirmed", "high"),
      verdict("refuted", "refuted", "high"),
    ];

    expect(runPolicyGate(detail(findings, verdicts)).counts).toEqual({
      critical: 1,
      high: 1,
      medium: 1,
      low: 1,
      total: 4,
    });
  });

  test("returns empty decisions for an empty review", () => {
    expect(runPolicyGate(detail([]))).toEqual({
      toPost: [],
      uiOnly: [],
      suppressed: [],
      counts: { critical: 0, high: 0, medium: 0, low: 0, total: 0 },
      overflow: 0,
    });
  });
});
