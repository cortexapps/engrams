import { describe, expect, test } from "bun:test";

import {
  classifyGithubEvent,
  parseReviewCommand,
} from "../integrations/github-webhook.ts";

/** A `pull_request` delivery shaped like the real thing: GitHub sends the whole
 *  pull-request object, which is what lets a review start without an API call
 *  (ADR 0100 d11). `pullRequestOverrides` replaces fields inside that object. */
function pullRequest(
  action: string,
  pullRequestOverrides: Record<string, unknown> = {},
) {
  return JSON.stringify({
    action,
    number: 100,
    repository: { full_name: "openai/engrams" },
    pull_request: {
      id: 2158810101,
      node_id: "PR_kwDOJ1",
      html_url: "https://github.com/openai/engrams/pull/100",
      title: "Bump quinn-proto from 0.11.14 to 0.11.16",
      user: { login: "dependabot[bot]" },
      state: "open",
      merged: false,
      head: { sha: "head-sha", ref: "dependabot/cargo/quinn-proto-0.11.16" },
      base: { sha: "base-sha", ref: "main" },
      draft: false,
      additions: 12,
      deletions: 4,
      changed_files: 2,
      updated_at: "2026-07-21T12:34:56Z",
      ...pullRequestOverrides,
    },
  });
}

const DELIVERED_PR = {
  providerId: "2158810101",
  url: "https://github.com/openai/engrams/pull/100",
  providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
  title: "Bump quinn-proto from 0.11.14 to 0.11.16",
  author: "dependabot[bot]",
  state: "open",
  headBranch: "dependabot/cargo/quinn-proto-0.11.16",
  baseBranch: "main",
  additions: 12,
  deletions: 4,
  changedFiles: 2,
};

describe("classifyGithubEvent", () => {
  test("classifies each supported pull_request action", () => {
    for (
      const action of [
        "opened",
        "synchronize",
        "ready_for_review",
        "closed",
        "reopened",
        "edited",
        "converted_to_draft",
      ] as const
    ) {
      expect(classifyGithubEvent("pull_request", pullRequest(action))).toEqual({
        kind: "pull_request",
        action,
        repo: "openai/engrams",
        prNumber: 100,
        headSha: "head-sha",
        baseSha: "base-sha",
        baseRef: "main",
        draft: false,
        pr: DELIVERED_PR,
      });
    }
  });

  test("a delivery missing base.sha still classifies, just without the shortcut", () => {
    // base.sha is an optimisation: it saves the pass an API call. Treating it as
    // required would silently drop a real review trigger, which is a far worse
    // failure than one extra request.
    const event = classifyGithubEvent(
      "pull_request",
      pullRequest("opened", { base: { ref: "main" } }),
    );
    expect(event.kind).toBe("pull_request");
    expect(event).toMatchObject({ baseSha: null, baseRef: "main" });
  });

  test("a draft PR reports draft state, not just the draft flag", () => {
    const event = classifyGithubEvent("pull_request", pullRequest("opened", { draft: true }));
    expect(event).toMatchObject({ draft: true, pr: { state: "draft" } });
  });

  test("a merged PR folds GitHub's state/merged pair into one axis", () => {
    const event = classifyGithubEvent(
      "pull_request",
      pullRequest("closed", { state: "closed", merged: true }),
    );
    expect(event).toMatchObject({ pr: { state: "merged" } });
  });

  test("classifies issue comments only when the issue is a PR", () => {
    const base = {
      action: "created",
      repository: { full_name: "openai/engrams" },
      comment: {
        id: 42,
        body: "@engrams review",
        author_association: "MEMBER",
      },
      sender: { type: "User" },
    };
    expect(classifyGithubEvent("issue_comment", JSON.stringify({
      ...base,
      issue: { number: 100, pull_request: { url: "https://api.github.test/pr/100" } },
    }))).toEqual({
      kind: "comment",
      repo: "openai/engrams",
      prNumber: 100,
      body: "@engrams review",
      commentId: "42",
      authorAssociation: "MEMBER",
      senderType: "User",
    });
    expect(classifyGithubEvent("issue_comment", JSON.stringify({
      ...base,
      issue: { number: 100 },
    }))).toEqual({ kind: "ignore" });
  });

  test("classifies review comments", () => {
    expect(classifyGithubEvent("pull_request_review_comment", JSON.stringify({
      action: "created",
      repository: { full_name: "openai/engrams" },
      pull_request: { number: 100 },
      comment: {
        id: "rc-1",
        body: "@engrams stop",
        author_association: "COLLABORATOR",
      },
      sender: { type: "Bot" },
    }))).toEqual({
      kind: "comment",
      repo: "openai/engrams",
      prNumber: 100,
      body: "@engrams stop",
      commentId: "rc-1",
      authorAssociation: "COLLABORATOR",
      senderType: "Bot",
    });
  });

  test("classifies ping and ignores junk or malformed payloads", () => {
    expect(classifyGithubEvent("ping", "{}")).toEqual({ kind: "ping" });
    expect(classifyGithubEvent("push", "{}")).toEqual({ kind: "ignore" });
    expect(classifyGithubEvent("pull_request", "not json")).toEqual({ kind: "ignore" });
    expect(classifyGithubEvent("pull_request", JSON.stringify({ action: "opened" })))
      .toEqual({ kind: "ignore" });
  });
});

describe("parseReviewCommand", () => {
  // The mention is the CONFIGURED App handle, not a hardcoded name.
  const handle = "acme-reviewer";

  test("parses review, focused review, stop, and fix against the App handle", () => {
    expect(parseReviewCommand("@acme-reviewer review", handle)).toEqual({ kind: "review" });
    expect(parseReviewCommand("@ACME-REVIEWER review focus on auth", handle))
      .toEqual({ kind: "review", focus: "focus on auth" });
    expect(parseReviewCommand("@acme-reviewer stop", handle)).toEqual({ kind: "stop" });
    expect(parseReviewCommand("@acme-reviewer fix this", handle))
      .toEqual({ kind: "fix", text: "this" });
  });

  test("tolerates a [bot] suffix on the mention and on the configured handle", () => {
    expect(parseReviewCommand("@acme-reviewer[bot] review", handle)).toEqual({ kind: "review" });
    expect(parseReviewCommand("@acme-reviewer review", "@acme-reviewer[bot]"))
      .toEqual({ kind: "review" });
  });

  test("does not match a different handle, and a blank handle disables commands", () => {
    expect(parseReviewCommand("@engrams review", handle)).toBeNull();
    expect(parseReviewCommand("@someone-else review", handle)).toBeNull();
    expect(parseReviewCommand("@acme-reviewer review", "")).toBeNull();
  });

  test("allows leading prose on its own line and rejects non-commands", () => {
    expect(parseReviewCommand("Please take another look.\n  @acme-reviewer review auth  ", handle))
      .toEqual({ kind: "review", focus: "auth" });
    expect(parseReviewCommand("hello @acme-reviewer review", handle)).toBeNull();
    expect(parseReviewCommand("@acme-reviewer dance", handle)).toBeNull();
    expect(parseReviewCommand("@acme-reviewer stop now", handle)).toBeNull();
  });

  test("strips control characters and caps focus at 500 characters", () => {
    const parsed = parseReviewCommand(`@acme-reviewer review auth\u0000${"x".repeat(600)}`, handle);
    expect(parsed?.kind).toBe("review");
    if (parsed?.kind !== "review") throw new Error("expected review command");
    expect(parsed.focus).not.toContain("\u0000");
    expect(parsed.focus).toHaveLength(500);
  });
});
