import { describe, expect, test } from "bun:test";

import type {
  ReviewDetail,
  ReviewFindingInput,
  ReviewFindingRow,
  ReviewRow,
  ReviewStore,
  ReviewVerdictInput,
} from "../../db/reviews.ts";
import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry, type ToolContext } from "../registry.ts";
import { PR_REVIEW_CAPABILITY, registerReviewTools } from "../review.ts";

const REVIEW_ID = "00000000-0000-4000-8000-000000000001";
const FINDING_ID = "00000000-0000-4000-8000-000000000002";
const OTHER_FINDING_ID = "00000000-0000-4000-8000-000000000003";

function reviewRow(overrides: Partial<ReviewRow> = {}): ReviewRow {
  return {
    id: REVIEW_ID,
    repo: "openai/engrams",
    prNumber: 100,
    taskId: "task-1",
    headSha: "head",
    baseSha: "base",
    trigger: "dispatch",
    status: "finding",
    githubReviewId: null,
    summaryMd: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
    ...overrides,
  };
}

function findingRow(overrides: Partial<ReviewFindingRow> = {}): ReviewFindingRow {
  return {
    id: FINDING_ID,
    reviewId: REVIEW_ID,
    path: "src/index.ts",
    startLine: 10,
    endLine: 12,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "high",
    confidence: "high",
    title: "Wrong branch",
    bodyMd: "This branch returns the wrong result.",
    suggestedFix: null,
    evidence: ["src/index.ts"],
    state: "candidate",
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: "finder-call",
    createdAt: new Date(0),
    ...overrides,
  };
}

function fakeReviewStore(options: {
  active?: ReviewRow | null;
  detail?: ReviewDetail | null;
} = {}) {
  const findings: ReviewFindingInput[] = [];
  const verdicts: ReviewVerdictInput[] = [];
  const summaries: Array<{ reviewId: string; summaryMd: string }> = [];
  const active = options.active === undefined ? reviewRow() : options.active;
  const detail = options.detail === undefined
    ? { review: reviewRow(), findings: [], verdicts: [] }
    : options.detail;
  const store: ReviewStore = {
    async createReview() {
      return REVIEW_ID;
    },
    async getReview() {
      return detail;
    },
    async listReviews() {
      return [];
    },
    async getActiveReviewForTask() {
      return active;
    },
    async getActiveReviewForPr() {
      return null;
    },
    async insertFinding(input) {
      findings.push(input);
      return { id: FINDING_ID, replayed: false };
    },
    async insertVerdict(input) {
      verdicts.push(input);
      return { id: "00000000-0000-4000-8000-000000000004", replayed: false };
    },
    async setFinderSummary(reviewId, summaryMd) {
      summaries.push({ reviewId, summaryMd });
    },
    async updateReviewStatus() {},
  };
  return { store, findings, verdicts, summaries };
}

function context(toolName: string): ToolContext {
  return {
    sessionId: "session-1",
    taskId: "task-1",
    capabilities: [PR_REVIEW_CAPABILITY],
    toolCallId: "call-1",
    toolName,
  };
}

function reviewRegistry(store: ReviewStore) {
  const registry = createToolRegistry();
  registerReviewTools(registry, { reviews: store });
  return registry;
}

describe("review tools", () => {
  test("capability gate excludes all review tools from an ungranted manifest", () => {
    const fake = fakeReviewStore();
    const registry = reviewRegistry(fake.store);

    expect(compileToolManifest(registry, [])).toEqual([]);
    expect(compileToolManifest(registry, [PR_REVIEW_CAPABILITY]).map((tool) => tool.name)).toEqual([
      "submit_finding",
      "finder_done",
      "submit_verdict",
    ]);
  });

  test("submit_finding schema rejects categories outside the fixed taxonomy", () => {
    const tool = reviewRegistry(fakeReviewStore().store).get("submit_finding");
    if (!tool) throw new Error("submit_finding not registered");

    expect(tool.input.safeParse({
      path: "src/index.ts",
      category: "style",
      severity: "high",
      confidence: "high",
      title: "Bad category",
      body_md: "Body",
      evidence: [],
    }).success).toBe(false);
  });

  test("no active review returns the protocol error object", async () => {
    const registry = reviewRegistry(fakeReviewStore({ active: null }).store);
    const tool = registry.get("submit_finding");
    if (!tool || tool.handling !== "handled") throw new Error("submit_finding not registered");
    const args = tool.input.parse({
      path: "src/index.ts",
      category: "functional-correctness",
      severity: "high",
      confidence: "high",
      title: "Wrong branch",
      body_md: "Body",
      evidence: ["src/index.ts"],
    });

    await expect(tool.handler(context(tool.name), args)).resolves.toEqual({
      error: "no active review for this session",
    });
  });

  test("finder handlers persist a candidate and its phase summary", async () => {
    const fake = fakeReviewStore();
    const registry = reviewRegistry(fake.store);
    const submit = registry.get("submit_finding");
    const done = registry.get("finder_done");
    if (!submit || submit.handling !== "handled" || !done || done.handling !== "handled") {
      throw new Error("finder tools not registered");
    }

    const result = await submit.handler(context(submit.name), submit.input.parse({
      path: "src/index.ts",
      start_line: 10,
      end_line: 12,
      side: "RIGHT",
      category: "functional-correctness",
      severity: "high",
      confidence: "high",
      title: "Wrong branch",
      body_md: "Body",
      suggested_fix: "Return the other value.",
      evidence: ["src/index.ts", "src/caller.ts"],
    }));
    await done.handler(context(done.name), done.input.parse({ summary_md: "One candidate." }));

    expect(result).toEqual({ recorded: true, finding_id: FINDING_ID });
    expect(fake.findings).toEqual([expect.objectContaining({
      reviewId: REVIEW_ID,
      state: "candidate",
      sessionId: "session-1",
      toolCallId: "call-1",
    })]);
    expect(fake.summaries).toEqual([{ reviewId: REVIEW_ID, summaryMd: "One candidate." }]);
  });

  test("submit_verdict rejects a finding owned by another review", async () => {
    const fake = fakeReviewStore({
      detail: { review: reviewRow(), findings: [findingRow()], verdicts: [] },
    });
    const tool = reviewRegistry(fake.store).get("submit_verdict");
    if (!tool || tool.handling !== "handled") throw new Error("submit_verdict not registered");

    const result = await tool.handler(context(tool.name), tool.input.parse({
      finding_id: OTHER_FINDING_ID,
      verdict: "refuted",
      confidence: "high",
      reasoning: "The alleged branch is unreachable.",
    }));

    expect(result).toEqual({ error: "finding does not belong to the active review" });
    expect(fake.verdicts).toEqual([]);
  });
});
