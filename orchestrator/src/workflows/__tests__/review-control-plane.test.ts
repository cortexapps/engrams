import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { EnrollmentRow } from "../../db/enrollments.ts";
import type { ProfileRow, ProfileStore } from "../../db/profiles.ts";
import type {
  ReviewDetail,
  ReviewFindingRow,
  ReviewRow,
  ReviewStore,
  ReviewVerdictRow,
} from "../../db/reviews.ts";
import type { ReviewSessionStore } from "../../db/review-sessions.ts";
import type { GithubReviewPoster, PostReviewInput } from "../../reviews/github-review.ts";
import type { CreateSessionForExistingTaskParams } from "../../rpc/task-create.ts";
import {
  makeReviewControlPlane,
  ReviewSetupError,
  type ReviewSessionsClient,
} from "../review-control-plane.ts";

const active: ReviewRow = {
  id: "review-1",
  repo: "openai/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "0123456789abcdef0123456789abcdef01234567",
  baseSha: "",
  trigger: "opened",
  status: "queued",
  githubReviewId: null,
  summaryMd: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

function finding(
  id: string,
  state = "candidate",
): ReviewFindingRow {
  return {
    id,
    reviewId: active.id,
    path: "orchestrator/src/workflows/pr-review.ts",
    startLine: 42,
    endLine: 45,
    side: "RIGHT",
    category: "functional-correctness",
    severity: "high",
    confidence: "medium",
    title: "Retry skips a phase",
    bodyMd: "The second terminal event bypasses verification.",
    suggestedFix: null,
    evidence: ["orchestrator/src/workflows/pr-review.ts"],
    state,
    verdictReason: null,
    githubThreadId: null,
    resolution: null,
    sessionId: "finder-session",
    toolCallId: `call-${id}`,
    createdAt: new Date("2026-07-17T00:01:00Z"),
  };
}

function detail(findings: ReviewFindingRow[] = []): ReviewDetail {
  return { review: active, findings, verdicts: [] };
}

const reviewPostingNoops = {
  updateFindingState: async () => {},
  finalizeReview: async () => {},
};

function reviewSessionRecorder(order?: string[]): ReviewSessionStore & {
  calls: Array<[string, string, string]>;
  removes: string[];
} {
  const calls: Array<[string, string, string]> = [];
  const removes: string[] = [];
  return {
    calls,
    removes,
    async record(sessionId, reviewWorkflowId, role) {
      order?.push(`binding:${sessionId}`);
      calls.push([sessionId, reviewWorkflowId, role]);
    },
    async find() {
      return null;
    },
    async remove(sessionId) {
      removes.push(sessionId);
    },
  };
}

const reviewerProfile = (id: string): ProfileRow => ({
  id,
  name: "Reviewer",
  description: "",
  icon: "Bot",
  imageId: "image-1",
  harness: "claude",
  model: null,
  effort: null,
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: ["engram:pr_review"],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: false,
  portExposures: [],
  designation: "pr_reviewer",
  createdAt: new Date(0),
  updatedAt: new Date(0),
  deletedAt: null,
});

function profileLookup(designated: ProfileRow | null): Pick<
  ProfileStore,
  "getActive" | "getByDesignation"
> {
  return {
    getActive: async (id) => designated?.id === id ? designated : null,
    getByDesignation: async () => designated,
  };
}

function enrollment(repo: string, profileId: string | null): EnrollmentRow {
  return {
    repo,
    profileId,
    triggerMode: "auto",
    autofix: "off",
    createdAt: new Date(0),
    updatedAt: new Date(0),
  };
}

interface FakeSessionOptions {
  exitStatus?: number;
  stderr?: string;
  writeFailure?: string;
}

function fakeSessions(options: FakeSessionOptions = {}): ReviewSessionsClient & {
  execCalls: Array<{ sessionId: string; command: string }>;
  writeCalls: Array<Parameters<ReviewSessionsClient["writeFiles"]>[0]>;
  promptCalls: Array<Parameters<ReviewSessionsClient["sendPrompt"]>[0]>;
  deletedIds: string[];
} {
  const execCalls: Array<{ sessionId: string; command: string }> = [];
  const writeCalls: Array<Parameters<ReviewSessionsClient["writeFiles"]>[0]> = [];
  const promptCalls: Array<Parameters<ReviewSessionsClient["sendPrompt"]>[0]> = [];
  const deletedIds: string[] = [];
  return {
    execCalls,
    writeCalls,
    promptCalls,
    deletedIds,
    createSession: async () => ({ sessionId: "finder-session" }),
    deleteSession: async ({ sessionId }) => {
      deletedIds.push(sessionId);
      return {};
    },
    async *exec(req) {
      execCalls.push(req);
      if (options.stderr) {
        yield { event: { case: "stderr", value: new TextEncoder().encode(options.stderr) } };
      }
      yield {
        event: {
          case: "exit",
          value: { exitStatus: options.exitStatus ?? 0 },
        },
      };
    },
    async writeFiles(req) {
      writeCalls.push(req);
      return {
        results: req.files.map((file, index) => ({
          path: file.path,
          ok: options.writeFailure === undefined || index !== 0,
          ...(options.writeFailure !== undefined && index === 0
            ? { error: options.writeFailure }
            : {}),
        })),
      };
    },
    async sendPrompt(req) {
      promptCalls.push(req);
      return {};
    },
  };
}

describe("ReviewControlPlane", () => {
  test("dedupes against an active review without inserting a task", async () => {
    let taskInserts = 0;
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewPostingNoops,
        getActiveReviewForPr: async () => active,
        getReview: async () => detail(),
        createReview: async () => { throw new Error("unexpected create"); },
        updateReviewStatus: async () => {},
      },
      insertTask: async () => {
        taskInserts++;
        return "task-new";
      },
    });
    expect(await cp.ensureReviewRecord({
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
      trigger: "command",
    })).toEqual({ reviewId: active.id, taskId: active.taskId });
    expect(taskInserts).toBe(0);
  });

  test("creates a bare task/review and marks an active review halted", async () => {
    let current: ReviewRow | null = null;
    const creates: unknown[] = [];
    const statuses: unknown[] = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewPostingNoops,
        getActiveReviewForPr: async () => current,
        getReview: async () => detail(),
        createReview: async (input) => {
          creates.push(input);
          current = { ...active, id: "review-new", taskId: input.taskId };
          return "review-new";
        },
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
        },
      },
      insertTask: async () => "task-new",
    });
    expect(await cp.ensureReviewRecord({
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: "new-head",
      baseSha: "",
      trigger: "dispatch",
    })).toEqual({ reviewId: "review-new", taskId: "task-new" });
    expect(creates).toEqual([{
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: "new-head",
      baseSha: "",
      trigger: "dispatch",
      taskId: "task-new",
      status: "queued",
    }]);
    await cp.markReviewHalted(active.repo, active.prNumber);
    expect(statuses).toEqual([["review-new", "halted"]]);
  });

  test("creates the finder with the designated profile and clamped review policy", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const order: string[] = [];
    const reviewSessions = reviewSessionRecorder(order);
    const designated = reviewerProfile("profile-designated");
    const cp = makeReviewControlPlane({
      profiles: profileLookup(designated),
      enrollments: { get: async () => enrollment(active.repo, null) },
      createSessionForExistingTask: async (params) => {
        order.push("create");
        created.push(params);
        return { sessionId: "finder-session" };
      },
      reviewSessions,
      registerSessionListener: async (sessionId) => {
        order.push(`listener:${sessionId}`);
      },
    });

    expect(await cp.createFinderSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
      workflowId: "review-wf-1",
    })).toEqual({ sessionId: "finder-session" });
    expect(created[0]).toMatchObject({
      taskId: active.taskId,
      profileId: designated.id,
      role: "finder",
      capabilityOverride: [
        "engram:pr_review",
        `github:contents:read@${active.repo}`,
      ],
      networkOverride: {
        default: "deny",
        allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
        allowHostPatterns: [],
      },
      dropProfileSecretsAndEnv: true,
    });
    expect(created[0]?.extraCapabilities).toBeUndefined();
    expect(created[0]?.registerListener).toBeUndefined();
    expect(created[0]?.appendSystemPrompt).toContain("/workspace/.review/finder.md");
    expect(reviewSessions.calls).toEqual([
      ["finder-session", "review-wf-1", "finder"],
    ]);
    expect(order).toEqual([
      "create",
      "binding:finder-session",
      "listener:finder-session",
    ]);
  });

  test("an enrollment profile overrides the designated reviewer profile", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const cp = makeReviewControlPlane({
      profiles: profileLookup(reviewerProfile("profile-designated")),
      enrollments: { get: async () => enrollment(active.repo, "profile-enrolled") },
      createSessionForExistingTask: async (params) => {
        created.push(params);
        return { sessionId: "finder-session" };
      },
      reviewSessions: reviewSessionRecorder(),
      registerSessionListener: async () => {},
    });

    await cp.createFinderSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
      workflowId: "review-wf-1",
    });
    expect(created[0]?.profileId).toBe("profile-enrolled");
  });

  test("missing reviewer profile fails setup", async () => {
    const cp = makeReviewControlPlane({
      profiles: profileLookup(null),
      enrollments: { get: async () => enrollment(active.repo, null) },
    });

    await expect(cp.createFinderSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
      workflowId: "review-wf-1",
    })).rejects.toBeInstanceOf(ReviewSetupError);
  });

  test("creates the verifier with a clamped policy and records its workflow binding", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const order: string[] = [];
    const reviewSessions = reviewSessionRecorder(order);
    const designated = reviewerProfile("profile-designated");
    const cp = makeReviewControlPlane({
      profiles: profileLookup(designated),
      enrollments: { get: async () => enrollment(active.repo, null) },
      createSessionForExistingTask: async (params) => {
        order.push("create");
        created.push(params);
        return { sessionId: "verifier-session" };
      },
      reviewSessions,
      registerSessionListener: async (sessionId) => {
        order.push(`listener:${sessionId}`);
      },
    });

    expect(await cp.createVerifierSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
      workflowId: "review-wf-1",
    })).toEqual({ sessionId: "verifier-session" });
    expect(created[0]).toMatchObject({
      taskId: active.taskId,
      profileId: designated.id,
      role: "verifier",
      capabilityOverride: [
        "engram:pr_review",
        `github:contents:read@${active.repo}`,
      ],
      networkOverride: {
        default: "deny",
        allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
        allowHostPatterns: [],
      },
      dropProfileSecretsAndEnv: true,
    });
    expect(created[0]?.extraCapabilities).toBeUndefined();
    expect(created[0]?.registerListener).toBeUndefined();
    expect(created[0]?.appendSystemPrompt).toContain("submit_verdict");
    expect(reviewSessions.calls).toEqual([
      ["verifier-session", "review-wf-1", "verifier"],
    ]);
    expect(order).toEqual([
      "create",
      "binding:verifier-session",
      "listener:verifier-session",
    ]);
  });

  test("deletes a worker session and removes its review binding", async () => {
    const sessions = fakeSessions();
    const reviewSessions = reviewSessionRecorder();
    const cp = makeReviewControlPlane({ sessions, reviewSessions });

    await cp.deleteReviewSession("review-session");

    expect(sessions.deletedIds).toEqual(["review-session"]);
    expect(reviewSessions.removes).toEqual(["review-session"]);
  });

  test("removes the binding when the worker session is already absent", async () => {
    const sessions = {
      ...fakeSessions(),
      deleteSession: async () => {
        throw new ConnectError("missing", Code.NotFound);
      },
    };
    const reviewSessions = reviewSessionRecorder();
    const cp = makeReviewControlPlane({ sessions, reviewSessions });

    await expect(cp.deleteReviewSession("review-session")).resolves.toBeUndefined();
    expect(reviewSessions.removes).toEqual(["review-session"]);
  });

  test("bootstraps with an idempotent clone, checkout, and rendered finder files", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({ sessions });

    await cp.bootstrapFinderSession("finder-session", {
      repo: active.repo,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    expect(sessions.execCalls).toEqual([{
      sessionId: "finder-session",
      command: `rm -rf /workspace/engrams && git clone https://github.com/${active.repo}.git /workspace/engrams && git -C /workspace/engrams checkout ${active.headSha}`,
    }]);
    expect(sessions.writeCalls).toHaveLength(1);
    expect(sessions.writeCalls[0]?.files.map((file) => file.path)).toEqual([
      "/workspace/.review/finder.md",
      "/workspace/.review/lenses/functional-correctness.md",
    ]);
    expect(sessions.writeCalls[0]?.files.every((file) => file.mode === 0o644)).toBe(true);
    expect(new TextDecoder().decode(sessions.writeCalls[0]?.files[0]?.content)).toContain(
      "You are the finder",
    );
  });

  test("rejects a repo or head SHA that could inject into the bootstrap shell", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({ sessions });

    await expect(
      cp.bootstrapFinderSession("finder-session", { repo: "openai/engrams; rm -rf /", headSha: "" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    await expect(
      cp.bootstrapFinderSession("finder-session", { repo: active.repo, headSha: "$(touch pwned)" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    // Nothing was executed for the rejected inputs.
    expect(sessions.execCalls).toEqual([]);
  });

  test("a clone failure or failed staged file throws ReviewSetupError", async () => {
    const cloneFailure = makeReviewControlPlane({
      sessions: fakeSessions({ exitStatus: 1, stderr: "clone denied" }),
    });
    await expect(cloneFailure.bootstrapFinderSession("finder-session", {
      repo: active.repo,
      headSha: "",
    })).rejects.toThrow(/clone denied/);

    const writeFailure = makeReviewControlPlane({
      sessions: fakeSessions({ writeFailure: "disk full" }),
    });
    await expect(writeFailure.bootstrapFinderSession("finder-session", {
      repo: active.repo,
      headSha: "",
    })).rejects.toThrow(/disk full/);
  });

  test("bootstraps the verifier with its instructions and candidate findings", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getActiveReviewForPr: async () => active,
        getReview: async () => detail([
          finding("candidate-1"),
          finding("already-confirmed", "confirmed"),
        ]),
        createReview: async () => active.id,
        updateReviewStatus: async () => {},
      },
    });

    await cp.bootstrapVerifierSession("verifier-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: active.headSha,
    });

    expect(sessions.execCalls[0]).toEqual({
      sessionId: "verifier-session",
      command: `rm -rf /workspace/engrams && git clone https://github.com/${active.repo}.git /workspace/engrams && git -C /workspace/engrams checkout ${active.headSha}`,
    });
    const files = sessions.writeCalls[0]?.files ?? [];
    expect(files.map((file) => file.path)).toEqual([
      "/workspace/.review/verifier.md",
      "/workspace/.review/candidates.json",
    ]);
    expect(new TextDecoder().decode(files[0]?.content)).toContain(
      "You are the verifier",
    );
    expect(JSON.parse(new TextDecoder().decode(files[1]?.content))).toEqual([{
      id: "candidate-1",
      path: "orchestrator/src/workflows/pr-review.ts",
      start_line: 42,
      end_line: 45,
      side: "RIGHT",
      category: "functional-correctness",
      severity: "high",
      confidence: "medium",
      title: "Retry skips a phase",
      body_md: "The second terminal event bypasses verification.",
      evidence: ["orchestrator/src/workflows/pr-review.ts"],
    }]);
  });

  test("sends the stable finder prompt and marks the review finding", async () => {
    const sessions = fakeSessions();
    const statuses: Array<[string, string]> = [];
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getActiveReviewForPr: async () => null,
        getReview: async () => detail(),
        createReview: async () => "unused",
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
        },
      },
    });

    await cp.sendFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
      focus: "Check retry behavior",
    });

    expect(sessions.promptCalls[0]).toMatchObject({
      sessionId: "finder-session",
      promptId: `review:${active.id}:finder`,
    });
    expect(sessions.promptCalls[0]?.text).toContain("the PR diff");
    expect(sessions.promptCalls[0]?.text).toContain("Check retry behavior");
    expect(statuses).toEqual([[active.id, "finding"]]);
  });

  test("sends the verifier prompt and marks the review verifying", async () => {
    const sessions = fakeSessions();
    const statuses: Array<[string, string]> = [];
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getActiveReviewForPr: async () => null,
        getReview: async () => detail(),
        createReview: async () => "unused",
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
        },
      },
    });

    await cp.sendVerifierPrompt("verifier-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    });

    expect(sessions.promptCalls[0]).toMatchObject({
      sessionId: "verifier-session",
      promptId: `review:${active.id}:verifier`,
    });
    expect(sessions.promptCalls[0]?.text).toContain("candidates.json");
    expect(sessions.promptCalls[0]?.text).toContain("submit_verdict");
    expect(statuses).toEqual([[active.id, "verifying"]]);
  });

  test("posts folded review results and persists SHAs, dispositions, and GitHub review id", async () => {
    const confirmed = {
      ...finding("confirmed"),
      suggestedFix: "return afterVerification;",
    };
    const refuted = finding("refuted");
    const verdicts: ReviewVerdictRow[] = [
      {
        id: "verdict-confirmed",
        findingId: confirmed.id,
        verdict: "confirmed",
        confidence: "high",
        reasoning: "Confirmed from the retry branch.",
        sessionId: "verifier-session",
        toolCallId: "verdict-call-confirmed",
        createdAt: new Date(1),
      },
      {
        id: "verdict-refuted",
        findingId: refuted.id,
        verdict: "refuted",
        confidence: "high",
        reasoning: "The terminal guard prevents this path.",
        sessionId: "verifier-session",
        toolCallId: "verdict-call-refuted",
        createdAt: new Date(1),
      },
    ];
    const findingUpdates: Array<{
      id: string;
      state: string;
      opts?: { githubThreadId?: string; verdictReason?: string };
    }> = [];
    const finalizations: Array<{
      id: string;
      input: Parameters<ReviewStore["finalizeReview"]>[1];
    }> = [];
    const posted: PostReviewInput[] = [];
    const githubPoster: GithubReviewPoster = {
      fetchPrHeads: async () => ({ headSha: "live-head", baseSha: "live-base" }),
      alreadyPosted: async () => false,
      async postReview(input) {
        posted.push(input);
        return {
          githubReviewId: "github-review-42",
          posted: true,
          inlinePosted: true,
          summaryMd: input.buildSummary(true),
        };
      },
    };
    const cp = makeReviewControlPlane({
      reviews: {
        getActiveReviewForPr: async () => active,
        getReview: async () => ({ review: active, findings: [confirmed, refuted], verdicts }),
        createReview: async () => active.id,
        updateReviewStatus: async () => {},
        async updateFindingState(id, state, opts) {
          findingUpdates.push({ id, state, ...(opts ? { opts } : {}) });
        },
        async finalizeReview(id, input) {
          finalizations.push({ id, input });
        },
      },
      githubPoster,
    });

    await cp.postReviewResults(active.id);

    expect(posted).toHaveLength(1);
    expect(posted[0]).toMatchObject({
      repo: active.repo,
      prNumber: active.prNumber,
      commitId: "live-head",
      comments: [{
        findingId: confirmed.id,
        path: confirmed.path,
        startLine: 42,
        line: 45,
        side: "RIGHT",
      }],
    });
    expect(posted[0]?.comments[0]?.body).toContain(
      "```suggestion\nreturn afterVerification;\n```",
    );
    // The concise (inline) summary the poster would post ends with the marker.
    expect(posted[0]?.buildSummary(true).endsWith(`<!-- engrams-review:${active.id} -->`)).toBe(true);
    expect(
      String(finalizations.at(-1)?.input.summaryMd).endsWith(`<!-- engrams-review:${active.id} -->`),
    ).toBe(true);
    expect(findingUpdates).toEqual([
      { id: confirmed.id, state: "posted" },
      {
        id: refuted.id,
        state: "suppressed_refuted",
        opts: { verdictReason: "The terminal guard prevents this path." },
      },
    ]);
    expect(finalizations[0]).toEqual({
      id: active.id,
      input: {
        status: active.status,
        summaryMd: "",
        headSha: "live-head",
        baseSha: "live-base",
      },
    });
    expect(finalizations.at(-1)).toMatchObject({
      id: active.id,
      input: {
        status: "posted",
        githubReviewId: "github-review-42",
      },
    });
  });

  test("marker idempotency fills SHAs and marks posted without posting or refolding", async () => {
    const finalizations: Array<Parameters<ReviewStore["finalizeReview"]>[1]> = [];
    let postCalls = 0;
    let findingUpdates = 0;
    const cp = makeReviewControlPlane({
      reviews: {
        getActiveReviewForPr: async () => active,
        getReview: async () => detail([finding("candidate")]),
        createReview: async () => active.id,
        updateReviewStatus: async () => {},
        updateFindingState: async () => {
          findingUpdates++;
        },
        finalizeReview: async (_id, input) => {
          finalizations.push(input);
        },
      },
      githubPoster: {
        fetchPrHeads: async () => ({ headSha: "live-head", baseSha: "live-base" }),
        alreadyPosted: async () => true,
        postReview: async () => {
          postCalls++;
          return { posted: true, inlinePosted: true, summaryMd: "" };
        },
      },
    });

    await cp.postReviewResults(active.id);

    expect(postCalls).toBe(0);
    expect(findingUpdates).toBe(0);
    expect(finalizations).toEqual([
      {
        status: active.status,
        summaryMd: "",
        headSha: "live-head",
        baseSha: "live-base",
      },
      { status: "posted", summaryMd: "" },
    ]);
  });
});
