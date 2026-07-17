import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../../db/enrollments.ts";
import type { ProfileRow, ProfileStore } from "../../db/profiles.ts";
import type { ReviewRow } from "../../db/reviews.ts";
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
} {
  const execCalls: Array<{ sessionId: string; command: string }> = [];
  const writeCalls: Array<Parameters<ReviewSessionsClient["writeFiles"]>[0]> = [];
  const promptCalls: Array<Parameters<ReviewSessionsClient["sendPrompt"]>[0]> = [];
  return {
    execCalls,
    writeCalls,
    promptCalls,
    createSession: async () => ({ sessionId: "finder-session" }),
    deleteSession: async () => ({}),
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
        getActiveReviewForPr: async () => active,
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
        getActiveReviewForPr: async () => current,
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

  test("creates the finder with the designated profile and scoped clone capability", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const designated = reviewerProfile("profile-designated");
    const cp = makeReviewControlPlane({
      profiles: profileLookup(designated),
      enrollments: { get: async () => enrollment(active.repo, null) },
      createSessionForExistingTask: async (params) => {
        created.push(params);
        return { sessionId: "finder-session" };
      },
    });

    expect(await cp.createFinderSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
    })).toEqual({ sessionId: "finder-session" });
    expect(created[0]).toMatchObject({
      taskId: active.taskId,
      profileId: designated.id,
      role: "finder",
      extraCapabilities: [`github:contents:read@${active.repo}`],
    });
    expect(created[0]?.appendSystemPrompt).toContain("/workspace/.review/finder.md");
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
    });

    await cp.createFinderSession({
      reviewId: active.id,
      taskId: active.taskId,
      repo: active.repo,
      prNumber: active.prNumber,
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
    })).rejects.toBeInstanceOf(ReviewSetupError);
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

  test("sends the stable finder prompt and marks the review finding", async () => {
    const sessions = fakeSessions();
    const statuses: Array<[string, string]> = [];
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        getActiveReviewForPr: async () => null,
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
});
