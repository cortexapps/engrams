import { afterEach, describe, expect, test } from "bun:test";

import type { PrContext } from "../../../reviews/pr-context.ts";
import type { ReviewPostPayload } from "../../../reviews/control-plane.ts";
import { registerEngineBlocks } from "../blocks/index.ts";
import { getBlock } from "../blocks/registry.ts";
import {
  OPEN_REVIEW_PASS_TYPE,
  REVIEW_CLEANUP_TYPE,
  REVIEW_POLICY_GATE_TYPE,
  setReviewBlockDeps,
  type ReviewBlockControlPlane,
} from "../blocks/system/review.ts";
import { buildRunContext, type RunContext } from "../context.ts";
import type { EngineDeps } from "../deps.ts";
import { validateDefinition } from "../definition.ts";

registerEngineBlocks();

const FULL_PR: PrContext = {
  providerId: "2158810101",
  title: "Bump quinn-proto",
  author: "dependabot[bot]",
  state: "open",
  url: "https://github.com/openai/engrams/pull/100",
  providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
  headBranch: "dependabot/quinn",
  baseBranch: "main",
  additions: 12,
  deletions: 4,
  changedFiles: 2,
};
const HEAD = "0123456789abcdef0123456789abcdef01234567";
const BASE = "fedcba9876543210fedcba9876543210fedcba98";

function prConfig(pr: PrContext = FULL_PR): Record<string, unknown> {
  return {
    providerId: pr.providerId,
    title: pr.title,
    author: pr.author,
    state: pr.state,
    url: pr.url,
    providerUpdatedAt: pr.providerUpdatedAt?.toISOString() ?? null,
    headBranch: pr.headBranch,
    baseBranch: pr.baseBranch,
    additions: pr.additions,
    deletions: pr.deletions,
    changedFiles: pr.changedFiles,
  };
}

interface Fake {
  cp: ReviewBlockControlPlane;
  calls: string[];
  stamped: Array<{ reviewId: string; runId: string }>;
  targets: unknown[];
  passes: unknown[];
}

function fake(options: { deduplicate?: boolean; payload?: ReviewPostPayload } = {}): Fake {
  const calls: string[] = [];
  const stamped: Fake["stamped"] = [];
  const targets: unknown[] = [];
  const passes: unknown[] = [];
  const cp: ReviewBlockControlPlane = {
    async bootstrapFinderSession() {
      calls.push("bootstrapFinderSession");
    },
    async bootstrapVerifierSession() {
      calls.push("bootstrapVerifierSession");
    },
    async composeFinderPrompt() {
      calls.push("composeFinderPrompt");
      return { prompt: "finder prompt", mergeBase: BASE };
    },
    composeVerifierPrompt() {
      calls.push("composeVerifierPrompt");
      return { prompt: "verifier prompt" };
    },
    async markPhasePrompted(_reviewId, role) {
      calls.push(`markPhasePrompted:${role}`);
    },
    async resolvePrHeads() {
      calls.push("resolvePrHeads");
      return { headSha: HEAD, baseSha: BASE, pr: FULL_PR };
    },
    async resolveReviewTarget(input) {
      calls.push("resolveReviewTarget");
      targets.push(input);
      return { targetId: "target-1" };
    },
    async createReviewPass(input) {
      calls.push("createReviewPass");
      passes.push(input);
      if (options.deduplicate) return { kind: "deduplicated", reviewId: "old", taskId: "t-old" };
      return { kind: "created", reviewId: "review-1", taskId: "task-1" };
    },
    async decideReviewResults(reviewId) {
      calls.push(`decideReviewResults:${reviewId}`);
      return (
        options.payload ?? {
          review_id: reviewId,
          repo: "openai/engrams",
          pr_number: 100,
          commit_id: HEAD,
          summary_md: `Summary\n\n<!-- engrams-review:${reviewId} -->`,
          comments: [],
          to_post_count: 0,
          ui_only_count: 0,
        }
      );
    },
    async cleanupSupersededReview(reviewId) {
      calls.push(`cleanupSupersededReview:${reviewId}`);
    },
  };
  setReviewBlockDeps({
    controlPlane: () => cp,
    reviews: () => ({
      async setAutomationRunId(reviewId, runId) {
        stamped.push({ reviewId, runId });
      },
    }),
  });
  return { cp, calls, stamped, targets, passes };
}

afterEach(() => setReviewBlockDeps(null));

/** A real engine context (the executors read only runId + currentBlockId);
 * the deps are never touched by these blocks, so they can throw. */
function ctx(): RunContext {
  const unused = (): never => {
    throw new Error("review blocks must not touch engine deps");
  };
  const context = buildRunContext(
    "autorun:auto-1:github:d1",
    {
      definition: {
        engine: 1,
        trigger: { kind: "manual" },
        blocks: [],
        inputsSchema: [],
        settings: { endSessionsOnFinish: false },
      },
      inputs: {},
      automationId: "auto-1",
      automationName: "PR review",
      version: 1,
      trigger: { kind: "manual", receivedAt: "2026-08-21T00:00:00Z" },
      aliases: [],
      startedAtMs: 0,
    },
    {
      step: unused,
      recv: unused,
      store: new Proxy({}, { get: unused }) as EngineDeps["store"],
      sessions: new Proxy({}, { get: unused }) as EngineDeps["sessions"],
      clock: { nowMs: () => 0 },
    },
  );
  context.currentBlockId = "open";
  return context;
}

describe("system.open_review_pass", () => {
  test("skips the GitHub fetch when the trigger carried complete facts", async () => {
    const f = fake();
    const block = getBlock(OPEN_REVIEW_PASS_TYPE)!;
    const config = block.configSchema.parse({
      repo: "openai/engrams",
      prNumber: "100",
      trigger: "opened",
      headSha: HEAD,
      baseSha: BASE,
      pr: prConfig(),
    });
    const outcome = await block.execute!(config as never, ctx());

    expect(f.calls).toEqual(["resolveReviewTarget", "createReviewPass"]);
    expect(outcome).toMatchObject({
      kind: "ok",
      outputs: {
        review_id: "review-1",
        task_id: "task-1",
        target_id: "target-1",
        head_sha: HEAD,
        base_sha: BASE,
        pr_url: FULL_PR.url,
        deduplicated: false,
      },
    });
    expect(f.targets[0]).toMatchObject({ providerId: "2158810101", number: 100 });
    // Automation triggers dedupe an unchanged head; the pass carries the
    // per-pass facts from the delivered PR.
    expect(f.passes[0]).toMatchObject({
      deduplicateSameHead: true,
      headBranch: "dependabot/quinn",
      additions: 12,
    });
    expect(f.stamped).toEqual([{ reviewId: "review-1", runId: "autorun:auto-1:github:d1" }]);
  });

  test("resolves heads from GitHub for a comment command and does not dedupe a human trigger", async () => {
    const f = fake();
    const block = getBlock(OPEN_REVIEW_PASS_TYPE)!;
    const config = block.configSchema.parse({
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "command",
    });
    const outcome = await block.execute!(config as never, ctx());
    expect(outcome.kind).toBe("ok");
    expect(f.calls[0]).toBe("resolvePrHeads");
    expect(f.passes[0]).toMatchObject({ deduplicateSameHead: false, headSha: HEAD });
  });

  test("a same-head redelivery ends the run as filtered, not failed", async () => {
    const f = fake({ deduplicate: true });
    const block = getBlock(OPEN_REVIEW_PASS_TYPE)!;
    const config = block.configSchema.parse({
      repo: "openai/engrams",
      prNumber: 100,
      trigger: "synchronize",
      headSha: HEAD,
      baseSha: BASE,
      pr: prConfig(),
    });
    const outcome = await block.execute!(config as never, ctx());
    expect(outcome).toMatchObject({ kind: "end_run", status: "filtered" });
    expect(f.stamped).toEqual([]);
  });

  test("a forge answer without an id is a permanent block error", async () => {
    const f = fake();
    f.cp.resolvePrHeads = async () => ({
      headSha: HEAD,
      baseSha: BASE,
      pr: { ...FULL_PR, providerId: null },
    });
    const block = getBlock(OPEN_REVIEW_PASS_TYPE)!;
    const config = block.configSchema.parse({ repo: "openai/engrams", prNumber: 1, trigger: "retry" });
    const outcome = await block.execute!(config as never, ctx());
    expect(outcome).toMatchObject({ kind: "error", code: "review_no_provider_id", retryable: false });
  });

  test("rejects an unsafe repo or SHA at config time", () => {
    const block = getBlock(OPEN_REVIEW_PASS_TYPE)!;
    expect(() =>
      block.configSchema.parse({ repo: "openai/engrams; rm -rf /", prNumber: 1, trigger: "opened" }),
    ).toThrow();
    expect(() =>
      block.configSchema.parse({ repo: "a/b", prNumber: 1, trigger: "opened", headSha: "$(x)" }),
    ).toThrow();
  });
});

describe("system.review_policy_gate", () => {
  test("returns the post payload with the crash-safe marker as block outputs", async () => {
    const f = fake();
    const block = getBlock(REVIEW_POLICY_GATE_TYPE)!;
    const config = block.configSchema.parse({ reviewId: "review-1", sessionId: "verifier-1" });
    const outcome = await block.execute!(config as never, ctx());
    expect(f.calls).toEqual(["decideReviewResults:review-1"]);
    expect(outcome).toMatchObject({
      kind: "ok",
      outputs: { review_id: "review-1", commit_id: HEAD, to_post_count: 0 },
    });
    if (outcome.kind === "ok") {
      expect(String(outcome.outputs["summary_md"])).toContain("<!-- engrams-review:review-1 -->");
    }
  });
});

describe("system.review_cleanup", () => {
  test("tears down a superseded pass's worker", async () => {
    const f = fake();
    const block = getBlock(REVIEW_CLEANUP_TYPE)!;
    const config = block.configSchema.parse({ reviewId: "review-9", sessionId: "s-1" });
    const outcome = await block.execute!(config as never, ctx());
    expect(f.calls).toEqual(["cleanupSupersededReview:review-9"]);
    expect(outcome).toMatchObject({ kind: "ok", outputs: { cleaned: true } });
  });
});

describe("validator gate", () => {
  test("a built-in definition may reference the review system blocks; a user one may not", () => {
    const raw = {
      engine: 1,
      trigger: { kind: "manual" },
      blocks: [
        {
          id: "open",
          type: OPEN_REVIEW_PASS_TYPE,
          config: { repo: "openai/engrams", prNumber: 1, trigger: "opened" },
        },
        { id: "gate", type: REVIEW_POLICY_GATE_TYPE, config: { reviewId: "${{ steps.open.review_id }}" } },
      ],
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    };
    expect(validateDefinition(raw, { kind: "builtin" }).blocks).toHaveLength(2);
    expect(() => validateDefinition(raw, { kind: "user" })).toThrow(/reserved for built-in/);
  });
});
