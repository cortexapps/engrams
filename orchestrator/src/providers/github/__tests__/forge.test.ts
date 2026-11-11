import { describe, expect, test } from "bun:test";

import type { ReviewFindingRow } from "../../../db/reviews.ts";
import { ForgeRequestError, SAFE_REPO_PATH } from "../../../forges/forge.ts";
import {
  runIntegrationOp,
  type IntegrationOpRequest,
  type IntegrationOpResult,
} from "../../../integrations/run-op.ts";
import type { PolicyDecision } from "../../../reviews/policy-gate.ts";
import { buildInlineCommentBody, buildReviewSummary } from "../../../reviews/summary.ts";
import { makeGithubForge } from "../forge.ts";

const { isPermanentError } = makeGithubForge();

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

describe("the GitHub forge", () => {
  test("listReviewComments pages, tolerates junk entries, and keeps thread links", async () => {
    const fullPage = Array.from({ length: 100 }, (_, index) => ({
      id: index + 1,
      user: { login: "engrams-agent[bot]" },
      body: `finding ${index + 1}`,
      path: "src/index.ts",
    }));
    const fake = fakeRunOp([
      response(200, fullPage),
      response(200, [
        { id: 101, in_reply_to_id: 1, user: { login: "octocat" }, body: "reply" },
        "junk-entry",
        { body: "no id — dropped" },
      ]),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    const comments = await forge.listReviewComments("openai/engrams", 100);

    expect(fake.calls.map((call) => call.request.path)).toEqual([
      "/repos/openai/engrams/pulls/100/comments?per_page=100&page=1",
      "/repos/openai/engrams/pulls/100/comments?per_page=100&page=2",
    ]);
    expect(comments).toHaveLength(101);
    expect(comments[0]).toEqual({
      id: "1",
      inReplyToId: null,
      authorLogin: "engrams-agent[bot]",
      body: "finding 1",
      path: "src/index.ts",
    });
    expect(comments.at(-1)).toEqual({
      id: "101",
      inReplyToId: "1",
      authorLogin: "octocat",
      body: "reply",
      path: null,
    });
  });

  test("listReviewComments surfaces a GitHub failure with its status", async () => {
    const fake = fakeRunOp([response(502, { message: "bad gateway" })]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    await expect(forge.listReviewComments("openai/engrams", 100))
      .rejects.toThrow(/list pull request review comments failed with GitHub status 502/);
  });

  test("fetchPullRequest decodes the byte response and reads head/base SHAs", async () => {
    const fake = fakeRunOp([
      response(200, { head: { sha: "head-sha" }, base: { sha: "base-sha" } }),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    expect(await forge.fetchPullRequest("openai/engrams", 100)).toEqual({
      headSha: "head-sha",
      baseSha: "base-sha",
      pr: {
        providerId: null,
        url: null,
        providerUpdatedAt: null,
        title: null,
        author: null,
        headBranch: null,
        baseBranch: null,
        state: null,
        additions: null,
        deletions: null,
        changedFiles: null,
      },
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

  test("fetchPullRequest captures the PR's descriptive context (ADR 0100 d9)", async () => {
    const fake = fakeRunOp([
      response(200, {
        id: 2158810101,
        node_id: "PR_kwDOJ1",
        html_url: "https://github.com/openai/engrams/pull/881",
        title: "Bump quinn-proto from 0.11.14 to 0.11.16",
        user: { login: "dependabot[bot]" },
        state: "open",
        draft: false,
        merged: false,
        additions: 12,
        deletions: 4,
        changed_files: 2,
        updated_at: "2026-07-21T12:34:56Z",
        head: { sha: "head-sha", ref: "dependabot/cargo/quinn-proto-0.11.16" },
        base: { sha: "base-sha", ref: "main" },
      }),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    const { pr } = await forge.fetchPullRequest("openai/engrams", 881);
    expect(pr).toEqual({
      providerId: "2158810101",
      url: "https://github.com/openai/engrams/pull/881",
      providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
      title: "Bump quinn-proto from 0.11.14 to 0.11.16",
      author: "dependabot[bot]",
      headBranch: "dependabot/cargo/quinn-proto-0.11.16",
      baseBranch: "main",
      state: "open",
      additions: 12,
      deletions: 4,
      changedFiles: 2,
    });
  });

  // GitHub splits a PR's disposition across state/draft/merged; the reader folds
  // them into one axis, and the precedence matters (a merged PR is also closed).
  test.each([
    [{ state: "open", draft: true, merged: false }, "draft"],
    [{ state: "open", draft: false, merged: false }, "open"],
    [{ state: "closed", draft: false, merged: false }, "closed"],
    [{ state: "closed", draft: false, merged: true }, "merged"],
    // draft is meaningless once closed, and merged still outranks it
    [{ state: "closed", draft: true, merged: true }, "merged"],
  ])("folds %o into pr state %s", async (flags, expected) => {
    const fake = fakeRunOp([
      response(200, { ...flags, head: { sha: "h" }, base: { sha: "b" } }),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    const { pr } = await forge.fetchPullRequest("openai/engrams", 1);
    expect(pr.state).toBe(expected);
  });

  test("fetchPullRequest still resolves heads when descriptive fields are junk", async () => {
    const fake = fakeRunOp([
      response(200, {
        title: 42,
        user: "not-an-object",
        additions: -1,
        changed_files: 1.5,
        head: { sha: "head-sha", ref: "" },
        base: { sha: "base-sha" },
      }),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    // A review must never fail over a descriptive field: the SHAs come through
    // and every unusable field degrades to null on its own.
    const result = await forge.fetchPullRequest("openai/engrams", 1);
    expect(result.headSha).toBe("head-sha");
    expect(result.baseSha).toBe("base-sha");
    expect(result.pr).toEqual({
      providerId: null,
      url: null,
      providerUpdatedAt: null,
      title: null,
      author: null,
      headBranch: null,
      baseBranch: null,
      state: null,
      additions: null,
      deletions: null,
      changedFiles: null,
    });
  });

  test("classifies permanent failures without treating GitHub throttles as permanent", () => {
    expect(isPermanentError(
      new ForgeRequestError(
        403,
        "primary limit",
        JSON.stringify({ message: "API Rate Limit exceeded for installation" }),
      ),
    )).toBe(false);
    expect(isPermanentError(
      new ForgeRequestError(
        403,
        "secondary limit",
        JSON.stringify({ message: "You have exceeded a secondary rate limit" }),
      ),
    )).toBe(false);
    expect(isPermanentError(
      new ForgeRequestError(403, "forbidden", JSON.stringify({ message: "Forbidden" })),
    )).toBe(true);
    expect(isPermanentError(
      new ForgeRequestError(403, "not JSON", "gateway rejected the request"),
    )).toBe(true);
    for (const status of [401, 404, 410]) {
      expect(isPermanentError(
        new ForgeRequestError(status, "permanent", JSON.stringify({ message: "rate limit" })),
      )).toBe(true);
    }
    expect(isPermanentError(
      new ForgeRequestError(500, "transient", JSON.stringify({ message: "server error" })),
    )).toBe(false);
  });

  test("posts exact single- and multi-line anchors with a suggestion block", async () => {
    const fake = fakeRunOp([response(200, { id: 7654 })]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });
    const multiLineBody = buildInlineCommentBody(finding());

    expect(await forge.postReview({
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
      providerReviewId: "7654",
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
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    expect(await forge.postReview({
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
      providerReviewId: "7655",
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

  test("hasReviewWithMarker detects only the matching marker", async () => {
    const fake = fakeRunOp([
      response(200, [
        { body: "<!-- engrams-review:other-review -->" },
        { body: "Done\n<!-- engrams-review:review-1 -->" },
      ]),
      response(200, [{ body: "<!-- engrams-review:other-review -->" }]),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    expect(await forge.hasReviewWithMarker("openai/engrams", 100, "<!-- engrams-review:review-1 -->")).toBe(true);
    expect(await forge.hasReviewWithMarker("openai/engrams", 100, "<!-- engrams-review:review-1 -->")).toBe(false);
  });

  test("a 2xx review list that is not a list is a forge error, and not a permanent one", async () => {
    const fake = fakeRunOp([response(200, { message: "not a list" })]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    const error = await forge.hasReviewWithMarker("acme/app", 1, "marker").catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ForgeRequestError);
    expect(isPermanentError(error)).toBe(false);
  });

  test("findCommentWithMarker returns null for a list it cannot read", async () => {
    const empty: IntegrationOpResult = {
      status: 200,
      body: new Uint8Array(),
      contentType: "application/json",
      truncated: false,
    };
    const fake = fakeRunOp([empty, response(200, { message: "not a list" }), response(500, {})]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    for (let i = 0; i < 3; i++) {
      expect(await forge.findCommentWithMarker("acme/app", 1, "marker")).toBeNull();
    }
  });

  test("a safe repository path has no `.` or `..` name", () => {
    for (const repo of ["acme/app", "acme/group/app", "a.b/c_d-e", "acme/.config"]) {
      expect(SAFE_REPO_PATH.test(repo)).toBe(true);
    }
    for (const repo of ["acme", "acme/..", "../acme/app", "acme/./app", "acme/app/..", "acme/a b"]) {
      expect(SAFE_REPO_PATH.test(repo)).toBe(false);
    }
  });

  test("hasReviewWithMarker reads past a full first page to find the marker", async () => {
    const fullPage = Array.from({ length: 100 }, () => ({ body: "<!-- engrams-review:other -->" }));
    const fake = fakeRunOp([
      response(200, fullPage),
      response(200, [{ body: "Done\n<!-- engrams-review:review-1 -->" }]),
    ]);
    const forge = makeGithubForge({ runIntegrationOp: fake.run });

    expect(await forge.hasReviewWithMarker("openai/engrams", 100, "<!-- engrams-review:review-1 -->")).toBe(true);
    // It kept walking because page 1 was full (100) and lacked the marker.
    expect(fake.calls).toHaveLength(2);
    expect(fake.calls[0]?.request.path).toContain("page=1");
    expect(fake.calls[1]?.request.path).toContain("page=2");
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
