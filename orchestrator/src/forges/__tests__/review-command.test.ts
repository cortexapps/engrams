import { describe, expect, test } from "bun:test";

import { parseReviewCommand } from "../review-command.ts";

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
