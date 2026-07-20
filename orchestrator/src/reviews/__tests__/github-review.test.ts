import { describe, expect, test } from "bun:test";

import type { ReviewFindingRow } from "../../db/reviews.ts";
import {
  runIntegrationOp,
  type IntegrationOpRequest,
  type IntegrationOpResult,
} from "../../integrations/run-op.ts";
import {
  buildInlineCommentBody,
  buildReviewSummary,
  buildStatusComment,
  makeGithubReviewPoster,
} from "../github-review.ts";
import type { PolicyDecision } from "../policy-gate.ts";

interface RecordedCall {
  provider: string;
  request: IntegrationOpRequest;
}

function response(status: number, body: unknown): IntegrationOpResult {
  return {
    status,
    body: new TextEncoder().encode(JSON.stringify(body)),
    contentType: "application/json",
    truncated: false,
  };
}

function fakeRunOp(responses: IntegrationOpResult[]) {
  const calls: RecordedCall[] = [];
  const run: typeof runIntegrationOp = async (provider, request) => {
    calls.push({ provider, request });
    const next = responses.shift();
    if (!next) throw new Error("fake runIntegrationOp response queue exhausted");
    return next;
  };
  return { calls, run };
}

function finding(overrides: Partial<ReviewFindingRow> = {}): ReviewFindingRow {
  return {
    id: "finding-1",
    reviewId: "review-1",
    path: "src/index.ts",
    startLine: 10,
    endLine: 12,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "high",
    confidence: "high",
    title: "Return the validated value",
    bodyMd: "The unchecked value reaches the caller.",
    suggestedFix: "return validated;",
    evidence: ["src/index.ts"],
    state: "candidate",
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: "finding-call",
    createdAt: new Date(0),
    ...overrides,
  };
}

describe("GithubReviewPoster", () => {
  test("fetchPrHeads decodes the byte response and reads head/base SHAs", async () => {
    const fake = fakeRunOp([
      response(200, { head: { sha: "head-sha" }, base: { sha: "base-sha" } }),
    ]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.fetchPrHeads("openai/engrams", 100)).toEqual({
      headSha: "head-sha",
      baseSha: "base-sha",
    });
    expect(fake.calls).toEqual([{
      provider: "github",
      request: {
        method: "GET",
        path: "/repos/openai/engrams/pulls/100",
        contentType: "application/json",
      },
    }]);
  });

  test("posts exact single- and multi-line anchors with a suggestion block", async () => {
    const fake = fakeRunOp([response(200, { id: 7654 })]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });
    const multiLineBody = buildInlineCommentBody(finding());

    expect(await poster.postReview({
      repo: "openai/engrams",
      prNumber: 100,
      commitId: "head-sha",
      buildSummary: (inlinePosted) => (inlinePosted ? "inline summary" : "fallback summary"),
      comments: [
        {
          findingId: "single",
          path: "src/single.ts",
          line: 8,
          side: "LEFT",
          body: "Single line",
        },
        {
          findingId: "multi",
          path: "src/index.ts",
          startLine: 10,
          line: 12,
          side: "RIGHT",
          body: multiLineBody,
        },
      ],
    })).toEqual({
      githubReviewId: "7654",
      posted: true,
      inlinePosted: true,
      summaryMd: "inline summary",
    });

    expect(JSON.parse(String(fake.calls[0]?.request.body))).toEqual({
      commit_id: "head-sha",
      event: "COMMENT",
      body: "inline summary",
      comments: [
        {
          path: "src/single.ts",
          line: 8,
          side: "LEFT",
          body: "Single line",
        },
        {
          path: "src/index.ts",
          line: 12,
          side: "RIGHT",
          start_line: 10,
          start_side: "RIGHT",
          body: multiLineBody,
        },
      ],
    });
    expect(multiLineBody).toContain("```suggestion\nreturn validated;\n```");
  });

  test("a 422 retries once with the fuller fallback summary and no comments", async () => {
    const fake = fakeRunOp([
      response(422, { message: "line must be part of the diff" }),
      response(200, { id: 7655 }),
    ]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.postReview({
      repo: "openai/engrams",
      prNumber: 100,
      commitId: "head-sha",
      buildSummary: (inlinePosted) => (inlinePosted ? "inline summary" : "fallback summary"),
      comments: [{
        findingId: "finding-1",
        path: "src/index.ts",
        line: 12,
        side: "RIGHT",
        body: "Inline finding",
      }],
    })).toEqual({
      githubReviewId: "7655",
      posted: true,
      inlinePosted: false,
      summaryMd: "fallback summary",
    });

    expect(fake.calls).toHaveLength(2);
    const first = JSON.parse(String(fake.calls[0]?.request.body));
    const second = JSON.parse(String(fake.calls[1]?.request.body));
    // The first attempt carried the concise inline summary + comments; the
    // retry swaps in the fuller fallback body and drops the comments.
    expect(first.body).toBe("inline summary");
    expect(second).toEqual({
      commit_id: "head-sha",
      event: "COMMENT",
      body: "fallback summary",
    });
    expect(second).not.toHaveProperty("comments");
  });

  test("alreadyPosted detects only the matching hidden marker", async () => {
    const fake = fakeRunOp([
      response(200, [
        { body: "<!-- engrams-review:other-review -->" },
        { body: "Done\n<!-- engrams-review:review-1 -->" },
      ]),
      response(200, [{ body: "<!-- engrams-review:other-review -->" }]),
    ]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.alreadyPosted("openai/engrams", 100, "review-1")).toBe(true);
    expect(await poster.alreadyPosted("openai/engrams", 100, "review-1")).toBe(false);
  });

  test("alreadyPosted paginates past a full first page to find the marker (#764-3)", async () => {
    const fullPage = Array.from({ length: 100 }, () => ({ body: "<!-- engrams-review:other -->" }));
    const fake = fakeRunOp([
      response(200, fullPage),
      response(200, [{ body: "Done\n<!-- engrams-review:review-1 -->" }]),
    ]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.alreadyPosted("openai/engrams", 100, "review-1")).toBe(true);
    // It kept walking because page 1 was full (100) and lacked the marker.
    expect(fake.calls).toHaveLength(2);
    expect(fake.calls[0]?.request.path).toContain("page=1");
    expect(fake.calls[1]?.request.path).toContain("page=2");
  });

  test("upsertStatusComment posts a fresh comment when no id is given", async () => {
    const fake = fakeRunOp([response(201, { id: 555 })]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.upsertStatusComment({
      repo: "openai/engrams",
      prNumber: 100,
      body: "👀 acknowledged",
    })).toEqual({ commentId: "555" });
    expect(fake.calls[0]?.request).toMatchObject({
      method: "POST",
      path: "/repos/openai/engrams/issues/100/comments",
    });
  });

  test("upsertStatusComment edits in place when the id is known", async () => {
    const fake = fakeRunOp([response(200, { id: 555 })]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.upsertStatusComment({
      repo: "openai/engrams",
      prNumber: 100,
      commentId: "555",
      body: "⏳ reviewing",
    })).toEqual({ commentId: "555" });
    expect(fake.calls[0]?.request).toMatchObject({
      method: "PATCH",
      path: "/repos/openai/engrams/issues/comments/555",
    });
  });

  test("upsertStatusComment reposts when the sticky comment was deleted (404)", async () => {
    const fake = fakeRunOp([
      response(404, { message: "Not Found" }),
      response(201, { id: 777 }),
    ]);
    const poster = makeGithubReviewPoster({ runIntegrationOp: fake.run });

    expect(await poster.upsertStatusComment({
      repo: "openai/engrams",
      prNumber: 100,
      commentId: "555",
      body: "⏳ reviewing",
    })).toEqual({ commentId: "777" });
    expect(fake.calls).toHaveLength(2);
    expect(fake.calls[0]?.request.method).toBe("PATCH");
    expect(fake.calls[1]?.request.method).toBe("POST");
  });

  test("buildStatusComment renders each lifecycle phase with the hidden marker", () => {
    const marker = "<!-- engrams-status:review-1 -->";
    expect(buildStatusComment({ reviewId: "review-1", phase: "acknowledged" }))
      .toBe(`👀 **engrams review** — acknowledged, queued.\n\n${marker}`);
    expect(buildStatusComment({ reviewId: "review-1", phase: "verifying", count: 3 }))
      .toContain("confirming 3 candidate findings…");
    const posted = buildStatusComment({
      reviewId: "review-1",
      phase: "posted",
      count: 1,
      reviewUrl: "https://engrams.example/reviews",
    });
    expect(posted).toContain("1 finding posted.");
    expect(posted).toContain("[View details](https://engrams.example/reviews)");
    expect(posted.endsWith(marker)).toBe(true);
  });

  test("summary preserves demoted findings and ends with the marker", () => {
    const item = {
      finding: finding(),
      state: "post",
      confidence: "high",
      verdictReason: null,
    };
    const decision: PolicyDecision = {
      toPost: [item],
      uiOnly: [],
      suppressed: [],
      counts: { critical: 0, high: 1, medium: 0, low: 0, total: 1 },
      overflow: 0,
    };

    const summary = buildReviewSummary({
      reviewId: "review-1",
      reviewUrl: "https://engrams.example/reviews/review-1",
      decision,
      inlinePosted: false,
    });
    expect(summary).toContain("`src/index.ts:L12`");
    expect(summary).toContain("[View the full engrams review](https://engrams.example/reviews/review-1)");
    expect(summary.endsWith("<!-- engrams-review:review-1 -->")).toBe(true);
  });
});
