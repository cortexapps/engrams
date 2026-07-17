import { describe, expect, test } from "bun:test";

import {
  classifyGithubEvent,
  parseEngramsCommand,
} from "../integrations/github-webhook.ts";

function pullRequest(action: string, overrides: Record<string, unknown> = {}) {
  return JSON.stringify({
    action,
    number: 100,
    repository: { full_name: "openai/engrams" },
    pull_request: {
      head: { sha: "head-sha" },
      base: { ref: "main" },
      draft: false,
    },
    ...overrides,
  });
}

describe("classifyGithubEvent", () => {
  test("classifies each supported pull_request action", () => {
    for (const action of ["opened", "synchronize", "ready_for_review", "closed"] as const) {
      expect(classifyGithubEvent("pull_request", pullRequest(action))).toEqual({
        kind: "pull_request",
        action,
        repo: "openai/engrams",
        prNumber: 100,
        headSha: "head-sha",
        baseRef: "main",
        draft: false,
      });
    }
  });

  test("classifies issue comments only when the issue is a PR", () => {
    const base = {
      action: "created",
      repository: { full_name: "openai/engrams" },
      comment: { id: 42, body: "@engrams review" },
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
      comment: { id: "rc-1", body: "@engrams stop" },
    }))).toEqual({
      kind: "comment",
      repo: "openai/engrams",
      prNumber: 100,
      body: "@engrams stop",
      commentId: "rc-1",
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

describe("parseEngramsCommand", () => {
  test("parses review, focused review, stop, and fix", () => {
    expect(parseEngramsCommand("@engrams review")).toEqual({ kind: "review" });
    expect(parseEngramsCommand("@ENGRAMS review focus on auth"))
      .toEqual({ kind: "review", focus: "focus on auth" });
    expect(parseEngramsCommand("@engrams stop")).toEqual({ kind: "stop" });
    expect(parseEngramsCommand("@engrams fix this"))
      .toEqual({ kind: "fix", text: "this" });
  });

  test("allows leading prose on its own line and rejects non-commands", () => {
    expect(parseEngramsCommand("Please take another look.\n  @engrams review auth  "))
      .toEqual({ kind: "review", focus: "auth" });
    expect(parseEngramsCommand("hello @engrams review")).toBeNull();
    expect(parseEngramsCommand("@engrams dance")).toBeNull();
    expect(parseEngramsCommand("@engrams stop now")).toBeNull();
  });

  test("strips control characters and caps focus at 500 characters", () => {
    const parsed = parseEngramsCommand(`@engrams review auth\u0000${"x".repeat(600)}`);
    expect(parsed?.kind).toBe("review");
    if (parsed?.kind !== "review") throw new Error("expected review command");
    expect(parsed.focus).not.toContain("\u0000");
    expect(parsed.focus).toHaveLength(500);
  });
});
