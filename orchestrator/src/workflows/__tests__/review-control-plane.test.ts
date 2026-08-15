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
import type {
  GithubReviewPoster,
  PostReviewInput,
} from "../../reviews/github-review.ts";
import type { PrContext } from "../../reviews/pr-context.ts";
import type { CreateSessionForExistingTaskParams } from "../../rpc/task-create.ts";
import {
  makeReviewControlPlane,
  ReviewSetupError,
  type ReviewSessionsClient,
} from "../review-control-plane.ts";

/** A PR whose descriptive context is unavailable — what every review recorded
 *  before ADR 0100 decision 9 looks like, and what a junk payload degrades to. */
const NO_PR_CONTEXT: PrContext = {
  providerId: null,
  url: null,
  title: null,
  author: null,
  headBranch: null,
  baseBranch: null,
  state: null,
  additions: null,
  deletions: null,
  changedFiles: null,
  providerUpdatedAt: null,
};

const active: ReviewRow = {
  id: "review-1",
  targetId: "target-1",
  provider: "github",
  repo: "openai/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "0123456789abcdef0123456789abcdef01234567",
  baseSha: "",
  trigger: "opened",
  status: "queued",
  githubReviewId: null,
  statusCommentId: null,
  finderSessionId: null,
  verifierSessionId: null,
  summaryMd: null,
  providerId: null,
  prUrl: null,
  prTitle: null,
  prAuthor: null,
  headBranch: null,
  baseBranch: null,
  prState: null,
  additions: null,
  deletions: null,
  changedFiles: null,
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
  finalizeReview: async () => true,
  setStatusCommentId: async () => {},
  setReviewSessionId: async () => {},
  recordEvent: async () => {},
  claimTargetId: async () => null,
  listPriorPasses: async () => [],
  upsertTarget: async () => ({ id: "target-1" }),
  beginReviewPass: async () => ({
    kind: "created" as const,
    reviewId: "review-stub",
    taskId: "task-stub",
  }),
  updateReviewPassContext: async () => true,
};

// A full ReviewControlPlaneStore of no-ops for the session-lifecycle tests that
// don't otherwise care about the store (createFinderSession/createVerifierSession
// now stamp the session id on the review at kickoff).
const reviewStoreStub = {
  ...reviewPostingNoops,
  getReview: async () => detail(),
  updateReviewStatus: async () => true,
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
  integrationGrants: [{
    connectionId: "connection-engram",
    operation: "pr_review",
    resourceConstraints: [],
  }],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  repos: [],
  apps: [],
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
  stdout?: string;
  stderr?: string;
  writeFailure?: string;
}

interface RecordedFileWrite {
  sessionId: string;
  path: string;
  content: Uint8Array;
  mode?: number;
}

function fakeSessions(options: FakeSessionOptions = {}): ReviewSessionsClient & {
  execCalls: Array<Parameters<ReviewSessionsClient["exec"]>[0]>;
  cancelCalls: Array<Parameters<ReviewSessionsClient["cancelExec"]>[0]>;
  writeCalls: RecordedFileWrite[];
  promptCalls: Array<Parameters<ReviewSessionsClient["sendPrompt"]>[0]>;
  deletedIds: string[];
} {
  const execCalls: Array<Parameters<ReviewSessionsClient["exec"]>[0]> = [];
  const cancelCalls: Array<Parameters<ReviewSessionsClient["cancelExec"]>[0]> = [];
  const writeCalls: RecordedFileWrite[] = [];
  const promptCalls: Array<Parameters<ReviewSessionsClient["sendPrompt"]>[0]> = [];
  const deletedIds: string[] = [];
  return {
    execCalls,
    cancelCalls,
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
      yield {
        event: {
          case: "started",
          value: { execId: req.execId ?? "exec:server-minted" },
        },
      };
      if (options.stdout) {
        yield { event: { case: "stdout", value: new TextEncoder().encode(options.stdout) } };
      }
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
    async cancelExec(req) {
      cancelCalls.push(req);
      return {};
    },
    async writeFile(input) {
      let metadata: {
        sessionId: string;
        path: string;
        sizeBytes: bigint;
        sha256: string;
        mode?: number;
      } | undefined;
      const chunks: Uint8Array[] = [];
      for await (const item of input) {
        if (item.frame.case === "metadata") metadata = item.frame.value;
        else chunks.push(item.frame.value);
      }
      if (!metadata) throw new Error("missing file metadata");
      const content = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0));
      let offset = 0;
      for (const chunk of chunks) {
        content.set(chunk, offset);
        offset += chunk.byteLength;
      }
      writeCalls.push({
        sessionId: metadata.sessionId,
        path: metadata.path,
        content,
        mode: metadata.mode,
      });
      if (options.writeFailure !== undefined && writeCalls.length === 1) {
        throw new Error(options.writeFailure);
      }
      return { path: metadata.path, sizeBytes: metadata.sizeBytes, sha256: metadata.sha256 };
    },
    async sendPrompt(req) {
      promptCalls.push(req);
      return {};
    },
  };
}

describe("ReviewControlPlane", () => {
  test("resolves a target by claiming legacy identity before the guarded upsert", async () => {
    const calls: unknown[] = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        claimTargetId: async (input) => {
          calls.push(["claim", input]);
          return { id: "target-legacy" };
        },
        upsertTarget: async (input) => {
          calls.push(["upsert", input]);
          return { id: "target-legacy" };
        },
      },
    });
    const input = {
      provider: "github",
      providerId: "2158810101",
      repo: active.repo,
      number: active.prNumber,
      title: "Review target",
      author: "octocat",
      state: "open",
      url: "https://github.com/openai/engrams/pull/100",
      providerUpdatedAt: new Date("2026-07-17T00:00:00Z"),
    };

    expect(await cp.resolveReviewTarget(input)).toEqual({
      targetId: "target-legacy",
    });
    expect(calls).toEqual([
      ["claim", {
        provider: "github",
        providerId: "2158810101",
        repo: active.repo,
        number: active.prNumber,
      }],
      ["upsert", input],
    ]);
  });

  test("creates a pass through the atomic store seam and acknowledges pickup", async () => {
    const passInputs: unknown[] = [];
    const upserts: Array<{ commentId?: string; body: string }> = [];
    let persisted: string | undefined;
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        getReview: async () => detail(),
        beginReviewPass: async (input) => {
          passInputs.push(input);
          return {
            kind: "created",
            reviewId: active.id,
            taskId: active.taskId,
          };
        },
        setStatusCommentId: async (_id, commentId) => {
          persisted = commentId;
        },
      },
      githubPoster: {
        fetchPrContext: async () => ({ headSha: "h", baseSha: "b", pr: NO_PR_CONTEXT }),
        alreadyPosted: async () => false,
        listReviewComments: async () => [],
        postReview: async () => ({ posted: true, inlinePosted: true, summaryMd: "" }),
        async upsertStatusComment(input) {
          upserts.push({
            ...(input.commentId ? { commentId: input.commentId } : {}),
            body: input.body,
          });
          return { commentId: "gh-comment-1" };
        },
      },
    });
    const input = {
      provider: "github",
      targetId: active.targetId,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: active.baseSha,
      trigger: "opened",
      headBranch: active.headBranch,
      baseBranch: active.baseBranch,
      additions: active.additions,
      deletions: active.deletions,
      changedFiles: active.changedFiles,
      deduplicateSameHead: true,
    };

    expect(await cp.createReviewPass(input)).toEqual({
      kind: "created",
      reviewId: active.id,
      taskId: active.taskId,
    });
    expect(passInputs).toEqual([input]);
    // The ack is NOT part of pass creation. `beginReviewPass` is a single,
    // non-idempotent transaction, and ingress runs it as a retryable step — DBOS
    // re-invokes the whole callback on any throw. A GitHub call after the commit
    // would therefore turn its first transient error into a second pass. So the
    // ack is its own step, and creation must touch GitHub zero times.
    expect(upserts).toEqual([]);

    await cp.acknowledgeReviewPass(active.id);
    expect(upserts).toHaveLength(1);
    expect(upserts[0]?.commentId).toBeUndefined();
    expect(upserts[0]?.body).toContain("👀");
    expect(persisted).toBe("gh-comment-1");
  });

  test("a failing status ack never wedges pass creation", async () => {
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        getReview: async () => detail(),
        beginReviewPass: async () => ({
          kind: "created",
          reviewId: active.id,
          taskId: active.taskId,
        }),
      },
      githubPoster: {
        fetchPrContext: async () => ({ headSha: "h", baseSha: "b", pr: NO_PR_CONTEXT }),
        alreadyPosted: async () => false,
        listReviewComments: async () => [],
        postReview: async () => ({ posted: true, inlinePosted: true, summaryMd: "" }),
        upsertStatusComment: async () => { throw new Error("GitHub down"); },
      },
    });

    expect(await cp.createReviewPass({
      provider: "github",
      targetId: active.targetId,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: active.baseSha,
      trigger: "command",
      headBranch: null,
      baseBranch: null,
      additions: null,
      deletions: null,
      changedFiles: null,
      deduplicateSameHead: false,
    })).toEqual({
      kind: "created",
      reviewId: active.id,
      taskId: active.taskId,
    });

    // And the ack itself swallows the failure rather than propagating it. A 👀
    // comment is cosmetic; failing the review over it would be worse than losing
    // it. This is also what lets ingress run the ack as a plain step with no
    // error handling of its own.
    await expect(cp.acknowledgeReviewPass(active.id)).resolves.toBeUndefined();
  });

  test("creates the finder with the designated profile and clamped review policy", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const order: string[] = [];
    const reviewSessions = reviewSessionRecorder(order);
    const designated = reviewerProfile("profile-designated");
    const cp = makeReviewControlPlane({
      reviews: reviewStoreStub,
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

  test("stamps the worker session on the review at kickoff for a live watch link", async () => {
    const stamped: Array<[string, string, string]> = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        setReviewSessionId: async (reviewId, role, sessionId) => {
          stamped.push([reviewId, role, sessionId]);
        },
      },
      profiles: profileLookup(reviewerProfile("profile-designated")),
      enrollments: { get: async () => enrollment(active.repo, null) },
      createSessionForExistingTask: async () => ({ sessionId: "finder-session" }),
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

    expect(stamped).toEqual([[active.id, "finder", "finder-session"]]);
  });

  test("an enrollment profile overrides the designated reviewer profile", async () => {
    const created: CreateSessionForExistingTaskParams[] = [];
    const cp = makeReviewControlPlane({
      reviews: reviewStoreStub,
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
      reviews: reviewStoreStub,
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

  test("failReview tears down the worker session and removes its review binding", async () => {
    const sessions = fakeSessions();
    const reviewSessions = reviewSessionRecorder();
    const cp = makeReviewControlPlane({
      sessions,
      reviewSessions,
      reviews: { ...reviewStoreStub, getReview: async () => null },
    });

    await cp.failReview("review-1", { sessionId: "review-session" });

    expect(sessions.deletedIds).toEqual(["review-session"]);
    expect(reviewSessions.removes).toEqual(["review-session"]);
  });

  test("abandonIngress fails the pass when ingress had already created one", async () => {
    const statuses: string[] = [];
    const events: Array<{ kind: string; detail?: string }> = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        getReview: async () => null,
        updateReviewStatus: async (_reviewId, status) => {
          statuses.push(status);
          return true;
        },
        recordEvent: async (_reviewId, kind, detail) => {
          events.push({ kind, ...(detail === undefined ? {} : { detail }) });
        },
      },
    });

    await cp.abandonIngress(
      { provider: "github", repo: "openai/engrams", prNumber: 100, trigger: "retry" },
      "GitHub pull request request failed (404)",
      "review-1",
    );

    expect(statuses).toEqual(["failed"]);
    expect(events).toEqual([
      { kind: "failed", detail: "GitHub pull request request failed (404)" },
    ]);
  });

  test("abandonIngress touches no row when ingress never created a pass", async () => {
    let touched = 0;
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        getReview: async () => null,
        updateReviewStatus: async () => {
          touched++;
          return true;
        },
      },
    });

    await cp.abandonIngress(
      { provider: "github", repo: "openai/engrams", prNumber: 100, trigger: "command" },
      "github returned no id for openai/engrams#100",
    );

    expect(touched).toBe(0);
  });

  test("failReview still settles when the worker session is already absent", async () => {
    const sessions = {
      ...fakeSessions(),
      deleteSession: async () => {
        throw new ConnectError("missing", Code.NotFound);
      },
    };
    const reviewSessions = reviewSessionRecorder();
    const cp = makeReviewControlPlane({
      sessions,
      reviewSessions,
      reviews: { ...reviewStoreStub, getReview: async () => null },
    });

    await expect(cp.failReview("review-1", { sessionId: "review-session" }))
      .resolves.toBeUndefined();
    expect(reviewSessions.removes).toEqual(["review-session"]);
  });

  test("a late failReview cannot overwrite a superseded terminal row", async () => {
    let status = "superseded";
    const events: string[] = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewStoreStub,
        updateReviewStatus: async (_reviewId, attempted) => {
          expect(attempted).toBe("failed");
          return false;
        },
        recordEvent: async (_reviewId, kind) => {
          events.push(kind);
        },
      },
    });

    await cp.failReview(active.id, { reason: "late phase timeout" });

    expect(status).toBe("superseded");
    expect(events).toEqual([]);
  });

  test("bootstraps with an idempotent clone, checkout, and rendered finder files", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({ sessions });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    expect(sessions.execCalls).toEqual([{
      sessionId: "finder-session",
      command: `rm -rf /workspace/engrams && git clone https://github.com/${active.repo}.git /workspace/engrams && git -C /workspace/engrams checkout ${active.headSha}`,
      execId: "exec:finder-session:bootstrap-clone",
      stdoutOffset: 0n,
      stderrOffset: 0n,
      wake: true,
    }]);
    expect(sessions.writeCalls).toHaveLength(2);
    expect(sessions.writeCalls.map((file) => file.path)).toEqual([
      "/workspace/.review/finder.md",
      "/workspace/.review/lenses/functional-correctness.md",
    ]);
    expect(sessions.writeCalls.every((file) => file.mode === 0o644)).toBe(true);
    expect(new TextDecoder().decode(sessions.writeCalls[0]?.content)).toContain(
      "You are the finder",
    );
  });

  test("rejects a repo or head SHA that could inject into the bootstrap shell", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({ sessions });

    await expect(
      cp.bootstrapFinderSession("finder-session", { reviewId: active.id, repo: "openai/engrams; rm -rf /", headSha: "" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    await expect(
      cp.bootstrapFinderSession("finder-session", { reviewId: active.id, repo: active.repo, headSha: "$(touch pwned)" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    // Nothing was executed for the rejected inputs.
    expect(sessions.execCalls).toEqual([]);
  });

  test("a clone failure or failed staged file throws ReviewSetupError", async () => {
    const cloneFailure = makeReviewControlPlane({
      sessions: fakeSessions({ exitStatus: 1, stderr: "clone denied" }),
    });
    await expect(cloneFailure.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: "",
    })).rejects.toThrow(/clone denied/);

    const writeFailure = makeReviewControlPlane({
      sessions: fakeSessions({ writeFailure: "disk full" }),
    });
    await expect(writeFailure.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
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
        getReview: async () => detail([
          finding("candidate-1"),
          finding("already-confirmed", "confirmed"),
        ]),
        updateReviewStatus: async () => true,
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
      execId: "exec:verifier-session:bootstrap-clone",
      stdoutOffset: 0n,
      stderrOffset: 0n,
      wake: true,
    });
    const files = sessions.writeCalls;
    // The verifier gets the lens files too — it enforces each lens's
    // "Do not report" bar on the candidates.
    expect(files.map((file) => file.path)).toEqual([
      "/workspace/.review/verifier.md",
      "/workspace/.review/lenses/security-privacy.md",
      "/workspace/.review/lenses/stability-availability.md",
      "/workspace/.review/lenses/data-integrity-integration.md",
      "/workspace/.review/lenses/functional-correctness.md",
      "/workspace/.review/lenses/performance-scalability.md",
      "/workspace/.review/lenses/maintainability-quality.md",
      "/workspace/.review/candidates.json",
    ]);
    expect(new TextDecoder().decode(files[0]?.content)).toContain(
      "You are the verifier",
    );
    expect(JSON.parse(new TextDecoder().decode(files.at(-1)?.content))).toEqual([{
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

  test("stages prior-round findings and matched author replies for a re-review", async () => {
    const sessions = fakeSessions();
    const priorPass = {
      reviewId: "review-0",
      headSha: "f".repeat(40),
      trigger: "opened",
      status: "posted",
      createdAt: new Date("2026-07-16T00:00:00Z"),
      findings: [{ ...finding("prior-1"), state: "posted" }],
    };
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewStoreStub,
        listPriorPasses: async () => [priorPass],
      },
      githubPoster: {
        fetchPrContext: async () => ({ headSha: "h", baseSha: "b", pr: NO_PR_CONTEXT }),
        alreadyPosted: async () => false,
        // A root comment whose first line quotes the finding title, plus the
        // author's reply threaded onto it — the reply must ride along, matched
        // to the finding by that title.
        listReviewComments: async () => [
          {
            id: "900",
            inReplyToId: null,
            authorLogin: "engrams-agent[bot]",
            body: "**🎯 Functional Correctness · HIGH — Retry skips a phase**\n\nWHAT: …",
            path: "orchestrator/src/workflows/pr-review.ts",
          },
          {
            id: "901",
            inReplyToId: "900",
            authorLogin: "octocat",
            body: "Declining this one — the retry is fenced upstream.",
            path: "orchestrator/src/workflows/pr-review.ts",
          },
          {
            id: "902",
            inReplyToId: "899",
            authorLogin: "octocat",
            body: "Reply to a non-finding thread; must not ride along.",
            path: null,
          },
        ],
        postReview: async () => ({ posted: true, inlinePosted: true, summaryMd: "" }),
        upsertStatusComment: async () => ({ commentId: "status-1" }),
      },
    });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    const files = sessions.writeCalls;
    expect(files.map((file) => file.path)).toEqual([
      "/workspace/.review/finder.md",
      "/workspace/.review/lenses/functional-correctness.md",
      "/workspace/.review/prior-findings.json",
    ]);
    const staged = JSON.parse(new TextDecoder().decode(files.at(-1)?.content));
    expect(staged.prior_passes).toEqual([{
      review_id: "review-0",
      head_sha: priorPass.headSha,
      trigger: "opened",
      status: "posted",
      created_at: "2026-07-16T00:00:00.000Z",
      findings: [{
        title: "Retry skips a phase",
        path: "orchestrator/src/workflows/pr-review.ts",
        start_line: 42,
        end_line: 45,
        category: "functional-correctness",
        severity: "high",
        confidence: "medium",
        state: "posted",
        verdict_reason: null,
        body_md: "The second terminal event bypasses verification.",
      }],
    }]);
    expect(staged.author_replies).toEqual([{
      finding_title: "Retry skips a phase",
      author: "octocat",
      body: "Declining this one — the retry is fenced upstream.",
    }]);
  });

  test("a failing GitHub reply fetch degrades the prior context to DB-only", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewStoreStub,
        listPriorPasses: async () => [{
          reviewId: "review-0",
          headSha: "f".repeat(40),
          trigger: "opened",
          status: "posted",
          createdAt: new Date("2026-07-16T00:00:00Z"),
          findings: [finding("prior-1")],
        }],
      },
      githubPoster: {
        fetchPrContext: async () => ({ headSha: "h", baseSha: "b", pr: NO_PR_CONTEXT }),
        alreadyPosted: async () => false,
        listReviewComments: async () => { throw new Error("GitHub down"); },
        postReview: async () => ({ posted: true, inlinePosted: true, summaryMd: "" }),
        upsertStatusComment: async () => ({ commentId: "status-1" }),
      },
    });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    const files = sessions.writeCalls;
    const staged = JSON.parse(new TextDecoder().decode(files.at(-1)?.content));
    expect(staged.prior_passes).toHaveLength(1);
    expect(staged.author_replies).toEqual([]);
  });

  test("a first review stages no prior-findings file", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({ sessions, reviews: reviewStoreStub });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    const paths = sessions.writeCalls.map((file) => file.path);
    expect(paths).not.toContain("/workspace/.review/prior-findings.json");
  });

  test("sends the stable finder prompt and marks the review finding", async () => {
    const sessions = fakeSessions();
    const statuses: Array<[string, string]> = [];
    const events: Array<[string, string, string | undefined]> = [];
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => detail(),
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
          return true;
        },
        recordEvent: async (reviewId, kind, detail) => {
          events.push([reviewId, kind, detail]);
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
      promptId: `review:${active.id}:finder:finder-session`,
    });
    expect(sessions.promptCalls[0]?.text).toContain("the PR diff");
    expect(sessions.promptCalls[0]?.text).toContain("Check retry behavior");
    expect(statuses).toEqual([[active.id, "finding"]]);
    // The activity log gains a "reviewing" milestone so the UI can show the
    // finder is running, not just the coarse "finding" status.
    expect(events).toEqual([[active.id, "reviewing", undefined]]);
  });

  test("the finder anchors on the resolved merge base, never the base branch head", async () => {
    // input.baseSha is the base BRANCH's head, not the fork point — anchoring
    // a diff there shows base-branch commits gained since the fork as phantom
    // deletions in the PR (live: engrams#820 was reported as deleting a field
    // that main gained after the branch forked). sendFinderPrompt resolves the
    // TRUE merge base with `git merge-base` and hands THAT to the finder.
    const baseBranchHead = "b".repeat(40);
    const headSha = "e".repeat(40);
    const mergeBase = "a".repeat(40);
    const sessions = fakeSessions({ stdout: `${mergeBase}\n` });
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => detail(),
        updateReviewStatus: async () => true,
      },
    });
    await cp.sendFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha,
      baseSha: baseBranchHead,
    });
    // Resolution ran against the base-branch head + head in the checkout...
    expect(sessions.execCalls[0]?.command).toContain(
      `merge-base ${baseBranchHead} ${headSha}`,
    );
    // ...and the prompt anchors the diff on the resolved merge base (three-dot),
    // never on the base-branch head the phantom-deletion bug came from.
    expect(sessions.promptCalls[0]?.text).toContain(`${mergeBase}...${headSha}`);
    expect(sessions.promptCalls[0]?.text).not.toContain(baseBranchHead);
  });

  test("a synchronize re-review scopes the finder to the delta since the last posted head", async () => {
    const lastPostedHead = "c".repeat(40);
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => ({
          review: { ...active, trigger: "synchronize" },
          findings: [],
          verdicts: [],
        }),
        listPriorPasses: async () => [
          // Newest first: a failed pass must not become the delta anchor —
          // only the last POSTED head was actually seen by the author.
          {
            reviewId: "review-failed",
            headSha: "d".repeat(40),
            trigger: "synchronize",
            status: "failed",
            createdAt: new Date("2026-07-17T02:00:00Z"),
            findings: [],
          },
          {
            reviewId: "review-0",
            headSha: lastPostedHead,
            trigger: "opened",
            status: "posted",
            createdAt: new Date("2026-07-17T01:00:00Z"),
            findings: [],
          },
        ],
        updateReviewStatus: async () => true,
      },
    });

    await cp.sendFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    });

    const text = sessions.promptCalls[0]?.text ?? "";
    expect(text).toContain("automatic re-review");
    expect(text).toContain(`git diff ${lastPostedHead}...${active.headSha}`);
    expect(text).not.toContain("d".repeat(40));
    expect(text).toContain("/workspace/.review/prior-findings.json");
  });

  test("a human trigger keeps the full range even when prior passes exist", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        // `active` carries trigger "opened" — a human-owned trigger.
        getReview: async () => detail(),
        listPriorPasses: async () => [{
          reviewId: "review-0",
          headSha: "c".repeat(40),
          trigger: "opened",
          status: "posted",
          createdAt: new Date("2026-07-17T01:00:00Z"),
          findings: [],
        }],
        updateReviewStatus: async () => true,
      },
    });

    await cp.sendFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    });

    const text = sessions.promptCalls[0]?.text ?? "";
    expect(text).not.toContain("automatic re-review");
    // The prior-round context still rides along for a human-triggered pass.
    expect(text).toContain("/workspace/.review/prior-findings.json");
  });

  test("a retry finder session gets a DISTINCT prompt id", async () => {
    // The coordinator outbox is keyed globally by prompt_id with
    // ON CONFLICT DO NOTHING: if a retry session reuses the failed
    // attempt's prompt id, its enqueue silently no-ops and the fresh
    // session never receives a prompt (live wedge: review f33ad531).
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => detail(),
        updateReviewStatus: async () => true,
      },
    });
    const input = {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    };
    await cp.sendFinderPrompt("finder-attempt-1", input);
    await cp.sendFinderPrompt("finder-attempt-2", input);
    const ids = sessions.promptCalls.map((c) => c.promptId);
    expect(new Set(ids).size).toBe(2);
  });

  test("sends the verifier prompt and marks the review verifying", async () => {
    const sessions = fakeSessions();
    const statuses: Array<[string, string]> = [];
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => detail(),
        updateReviewStatus: async (reviewId, status) => {
          statuses.push([reviewId, status]);
          return true;
        },
      },
    });

    await cp.sendVerifierPrompt("verifier-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
    });

    expect(sessions.promptCalls[0]).toMatchObject({
      sessionId: "verifier-session",
      promptId: `review:${active.id}:verifier:verifier-session`,
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
      fetchPrContext: async () => ({
        headSha: "live-head",
        baseSha: "live-base",
        pr: NO_PR_CONTEXT,
      }),
      alreadyPosted: async () => false,
      listReviewComments: async () => [],
      upsertStatusComment: async () => ({ commentId: "status-1" }),
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
        ...reviewPostingNoops,
        getReview: async () => ({ review: active, findings: [confirmed, refuted], verdicts }),
        updateReviewStatus: async () => true,
        async updateFindingState(id, state, opts) {
          findingUpdates.push({ id, state, ...(opts ? { opts } : {}) });
        },
        async finalizeReview(id, input) {
          finalizations.push({ id, input });
          return true;
        },
      },
      githubPoster,
    });

    await cp.postReviewResults(active.id);

    expect(posted).toHaveLength(1);
    expect(posted[0]).toMatchObject({
      repo: active.repo,
      prNumber: active.prNumber,
      // #764-2: anchor to the REVIEWED head (the review row), never the live head.
      commitId: active.headSha,
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
        // Reviewed head kept; only the empty base is filled from the live fetch.
        headSha: active.headSha,
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

  test("marker idempotency fills SHAs and settles finding states without re-posting", async () => {
    const finalizations: Array<Parameters<ReviewStore["finalizeReview"]>[1]> = [];
    let postCalls = 0;
    const findingStates: Array<[string, string]> = [];
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => detail([finding("candidate")]),
        updateReviewStatus: async () => true,
        updateFindingState: async (id, state) => {
          findingStates.push([id, state]);
        },
        finalizeReview: async (_id, input) => {
          finalizations.push(input);
          return true;
        },
      },
      githubPoster: {
        fetchPrContext: async () => ({
          headSha: "live-head",
          baseSha: "live-base",
          pr: NO_PR_CONTEXT,
        }),
        alreadyPosted: async () => true,
        listReviewComments: async () => [],
        upsertStatusComment: async () => ({ commentId: "status-1" }),
        postReview: async () => {
          postCalls++;
          return { posted: true, inlinePosted: true, summaryMd: "" };
        },
      },
    });

    await cp.postReviewResults(active.id);

    // Recovery never re-posts to GitHub...
    expect(postCalls).toBe(0);
    // ...but it DOES run the idempotent finding-state updates the crashed
    // transaction never committed — an unverdicted candidate becomes ui_only,
    // so it can't stay stuck at `candidate` in the UI.
    expect(findingStates).toEqual([["candidate", "ui_only"]]);
    expect(finalizations).toEqual([
      {
        status: active.status,
        summaryMd: "",
        headSha: active.headSha,
        baseSha: "live-base",
      },
      { status: "posted", summaryMd: "" },
    ]);
  });

  test("posts against the reviewed head without a live fetch when both SHAs are stored", async () => {
    const reviewed: ReviewRow = { ...active, baseSha: "base-reviewed" };
    let fetchCalls = 0;
    let postedCommit: string | undefined;
    const cp = makeReviewControlPlane({
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => ({ review: reviewed, findings: [finding("candidate")], verdicts: [] }),
        updateReviewStatus: async () => true,
        updateFindingState: async () => {},
        finalizeReview: async () => true,
      },
      githubPoster: {
        fetchPrContext: async () => {
          fetchCalls++;
          return { headSha: "live-head", baseSha: "live-base", pr: NO_PR_CONTEXT };
        },
        alreadyPosted: async () => false,
        listReviewComments: async () => [],
        upsertStatusComment: async () => ({ commentId: "status-1" }),
        async postReview(input) {
          postedCommit = input.commitId;
          return { githubReviewId: "gh-1", posted: true, inlinePosted: true, summaryMd: input.buildSummary(true) };
        },
      },
    });

    await cp.postReviewResults(reviewed.id);

    // #764-2: both SHAs already stored → no live fetch, commit_id is the reviewed head.
    expect(fetchCalls).toBe(0);
    expect(postedCommit).toBe(reviewed.headSha);
  });
});
