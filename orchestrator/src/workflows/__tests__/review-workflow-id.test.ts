import { describe, expect, test } from "bun:test";

import {
  reviewHash,
  selectReviewWorkflowId,
} from "../review-workflow-id.ts";

describe("review workflow id", () => {
  test("hash is stable, repo/PR-specific, and 32 hex characters", () => {
    const hash = reviewHash("openai/engrams", 100);
    expect(hash).toMatch(/^[0-9a-f]{32}$/);
    expect(reviewHash("openai/engrams", 100)).toBe(hash);
    expect(reviewHash("openai/engrams", 101)).not.toBe(hash);
  });

  test("fresh or live review uses the base id", async () => {
    expect(await selectReviewWorkflowId("HASH", async () => false))
      .toBe("review:HASH");
  });

  test("terminal epochs bump to the first live/free successor", async () => {
    const terminal = new Set(["review:HASH", "review:HASH#1"]);
    expect(await selectReviewWorkflowId("HASH", async (id) => terminal.has(id)))
      .toBe("review:HASH#2");
  });
});
