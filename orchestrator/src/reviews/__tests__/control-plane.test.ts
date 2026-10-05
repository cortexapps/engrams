import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type {
  ReviewDetail,
  ReviewFindingRow,
  ReviewRow,
  ReviewStore,
  ReviewVerdictRow,
} from "../../db/reviews.ts";
import type { GithubReviewPoster } from "../github-review.ts";
import type { PrContext } from "../pr-context.ts";
import {
  makeReviewControlPlane,
  ReviewSetupError,
  type ReviewSessionsClient,
} from "../control-plane.ts";

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
  finderSessionId: null,
  verifierSessionId: null,
  automationRunId: null,
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
};

// A full ReviewControlPlaneStore of no-ops for the tests that don't otherwise
// care about the store.
const reviewStoreStub = {
  ...reviewPostingNoops,
  getReview: async () => detail(),
  updateReviewStatus: async () => true,
};

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

  test("creates a pass through the atomic store seam and touches GitHub zero times", async () => {
    const passInputs: unknown[] = [];
    let githubCalls = 0;
    const counting = (): never => {
      githubCalls++;
      throw new Error("GitHub must not be reached from createReviewPass");
    };
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
      },
      githubPoster: {
        fetchPrContext: async () => counting(),
        alreadyPosted: async () => counting(),
        listReviewComments: async () => counting(),
        postReview: async () => counting(),
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
    // `beginReviewPass` is a single, non-idempotent transaction, and the
    // review_open_pass block runs it as a retryable step — DBOS re-invokes
    // the whole callback on any throw. A GitHub call after the commit would
    // therefore turn its first transient error into a second pass. The
    // built-in's own `ack` block posts the status comment instead.
    expect(githubCalls).toBe(0);
  });

  test("failReview tears down the worker session", async () => {
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: { ...reviewStoreStub, getReview: async () => null },
    });

    await cp.failReview("review-1", { sessionId: "review-session" });

    expect(sessions.deletedIds).toEqual(["review-session"]);
  });

  test("failReview still settles when the worker session is already absent", async () => {
    const sessions = {
      ...fakeSessions(),
      deleteSession: async () => {
        throw new ConnectError("missing", Code.NotFound);
      },
    };
    const cp = makeReviewControlPlane({
      sessions,
      reviews: { ...reviewStoreStub, getReview: async () => null },
    });

    await expect(cp.failReview("review-1", { sessionId: "review-session" }))
      .resolves.toBeUndefined();
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
      prNumber: 41,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    expect(sessions.execCalls).toEqual([{
      sessionId: "finder-session",
      command: `rm -rf /workspace/engrams && git clone https://github.com/${active.repo}.git /workspace/engrams && (git -C /workspace/engrams checkout ${active.headSha} || (git -C /workspace/engrams fetch origin +refs/pull/41/head && git -C /workspace/engrams checkout ${active.headSha}))`,
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
      cp.bootstrapFinderSession("finder-session", { reviewId: active.id, repo: "openai/engrams; rm -rf /", prNumber: 41, headSha: "" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    await expect(
      cp.bootstrapFinderSession("finder-session", { reviewId: active.id, repo: active.repo, prNumber: 41, headSha: "$(touch pwned)" }),
    ).rejects.toBeInstanceOf(ReviewSetupError);
    // The PR number lands in the fallback fetch ref; a non-integer must
    // never reach the shell.
    await expect(
      cp.bootstrapFinderSession("finder-session", {
        reviewId: active.id,
        repo: active.repo,
        prNumber: 41.5,
        headSha: active.headSha,
      }),
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
      prNumber: 41,
      headSha: "",
    })).rejects.toThrow(/clone denied/);

    const writeFailure = makeReviewControlPlane({
      sessions: fakeSessions({ writeFailure: "disk full" }),
    });
    await expect(writeFailure.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: 41,
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
      prNumber: 41,
      headSha: active.headSha,
    });

    expect(sessions.execCalls[0]).toEqual({
      sessionId: "verifier-session",
      command: `rm -rf /workspace/engrams && git clone https://github.com/${active.repo}.git /workspace/engrams && (git -C /workspace/engrams checkout ${active.headSha} || (git -C /workspace/engrams fetch origin +refs/pull/41/head && git -C /workspace/engrams checkout ${active.headSha}))`,
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
      },
    });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: 41,
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
      },
    });

    await cp.bootstrapFinderSession("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: 41,
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
      prNumber: 41,
      headSha: active.headSha,
      enabledCategories: ["functional-correctness"],
    });

    const paths = sessions.writeCalls.map((file) => file.path);
    expect(paths).not.toContain("/workspace/.review/prior-findings.json");
  });

  test("composes the stable finder prompt; markPhasePrompted marks the review finding", async () => {
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

    const { prompt } = await cp.composeFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
      focus: "Check retry behavior",
    });
    await cp.markPhasePrompted(active.id, "finder");

    // The generic send_prompt block delivers the text; composition itself
    // sends nothing.
    expect(sessions.promptCalls).toEqual([]);
    expect(prompt).toContain("the PR diff");
    expect(prompt).toContain("Check retry behavior");
    expect(statuses).toEqual([[active.id, "finding"]]);
    // The activity log gains a "reviewing" milestone so the UI can show the
    // finder is running, not just the coarse "finding" status.
    expect(events).toEqual([[active.id, "reviewing", undefined]]);
  });

  test("the finder anchors on the resolved merge base, never the base branch head", async () => {
    // input.baseSha is the base BRANCH's head, not the fork point — anchoring
    // a diff there shows base-branch commits gained since the fork as phantom
    // deletions in the PR (live: engrams#820 was reported as deleting a field
    // that main gained after the branch forked). composeFinderPrompt resolves
    // the TRUE merge base with `git merge-base` and hands THAT to the finder.
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
    const { prompt } = await cp.composeFinderPrompt("finder-session", {
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
    expect(prompt).toContain(`${mergeBase}...${headSha}`);
    expect(prompt).not.toContain(baseBranchHead);
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

    const { prompt: text } = await cp.composeFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    });

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

    const { prompt: text } = await cp.composeFinderPrompt("finder-session", {
      reviewId: active.id,
      repo: active.repo,
      prNumber: active.prNumber,
      headSha: active.headSha,
      baseSha: "",
    });

    expect(text).not.toContain("automatic re-review");
    // The prior-round context still rides along for a human-triggered pass.
    expect(text).toContain("/workspace/.review/prior-findings.json");
  });

  test("composes the verifier prompt; markPhasePrompted marks the review verifying", async () => {
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

    const { prompt } = cp.composeVerifierPrompt({
      repo: active.repo,
      prNumber: active.prNumber,
    });
    await cp.markPhasePrompted(active.id, "verifier");

    expect(sessions.promptCalls).toEqual([]);
    expect(prompt).toContain("candidates.json");
    expect(prompt).toContain("submit_verdict");
    expect(statuses).toEqual([[active.id, "verifying"]]);
  });

  // ADR 0119 phase 4.2: the decision half without the GitHub post.
  test("decideReviewResults settles findings, finalizes, and returns the post payload without posting", async () => {
    const confirmed = { ...finding("confirmed"), suggestedFix: "return afterVerification;" };
    const refuted = finding("refuted");
    const verdicts: ReviewVerdictRow[] = [
      {
        id: "v-c",
        findingId: confirmed.id,
        verdict: "confirmed",
        confidence: "high",
        reasoning: "Confirmed from the retry branch.",
        sessionId: "verifier-session",
        toolCallId: "c1",
        createdAt: new Date(1),
      },
      {
        id: "v-r",
        findingId: refuted.id,
        verdict: "refuted",
        confidence: "high",
        reasoning: "The terminal guard prevents this path.",
        sessionId: "verifier-session",
        toolCallId: "c2",
        createdAt: new Date(1),
      },
    ];
    const reviewed: ReviewRow = { ...active, baseSha: "base-reviewed" };
    const findingUpdates: Array<{ id: string; state: string }> = [];
    const finalizations: Array<Parameters<ReviewStore["finalizeReview"]>[1]> = [];
    const events: string[] = [];
    let postCalls = 0;
    let fetchCalls = 0;
    const sessions = fakeSessions();
    const cp = makeReviewControlPlane({
      sessions,
      reviews: {
        ...reviewPostingNoops,
        getReview: async () => ({ review: reviewed, findings: [confirmed, refuted], verdicts }),
        updateReviewStatus: async () => true,
        async updateFindingState(id, state) {
          findingUpdates.push({ id, state });
        },
        async finalizeReview(_id, input) {
          finalizations.push(input);
          return true;
        },
        async recordEvent(_id, kind) {
          events.push(kind);
        },
      },
      githubPoster: {
        fetchPrContext: async () => {
          fetchCalls++;
          return { headSha: "live", baseSha: "live", pr: NO_PR_CONTEXT };
        },
        alreadyPosted: async () => false,
        listReviewComments: async () => [],
        async postReview() {
          postCalls++;
          throw new Error("must not post");
        },
      },
    });

    const payload = await cp.decideReviewResults(reviewed.id, { sessionId: "verifier-session" });

    // The verifier worker was retired first (best-effort), GitHub was never
    // posted to, and no live fetch was needed with both SHAs stored.
    expect(sessions.deletedIds).toEqual(["verifier-session"]);
    expect(postCalls).toBe(0);
    expect(fetchCalls).toBe(0);
    // Findings settled as on the inline-posted path; review finalized posted.
    expect(findingUpdates).toEqual([
      { id: confirmed.id, state: "posted" },
      { id: refuted.id, state: "suppressed_refuted" },
    ]);
    expect(finalizations.at(-1)).toMatchObject({ status: "posted" });
    expect(events).toContain("posted");
    // The payload is what github.post_pr_review posts: reviewed head, the
    // inline comment with its suggestion, and the crash-safe marker.
    expect(payload).toMatchObject({
      review_id: reviewed.id,
      repo: reviewed.repo,
      pr_number: reviewed.prNumber,
      commit_id: reviewed.headSha,
      to_post_count: 1,
      ui_only_count: 0,
      comments: [{ finding_id: confirmed.id, path: confirmed.path, start_line: 42, line: 45, side: "RIGHT" }],
    });
    expect(payload.comments[0]!.body).toContain("```suggestion\nreturn afterVerification;\n```");
    expect(payload.summary_md.endsWith(`<!-- engrams-review:${reviewed.id} -->`)).toBe(true);
    // The fallback body (GitHub refused an inline anchor → summary-only
    // review) re-quotes the confirmed finding so the PR still shows it; the
    // inline summary leaves the detail to the comment.
    expect(payload.fallback_summary_md).toContain(confirmed.title);
    expect(payload.fallback_summary_md).toContain(confirmed.bodyMd);
    expect(payload.summary_md).not.toContain(confirmed.bodyMd);
    expect(payload.fallback_summary_md.endsWith(`<!-- engrams-review:${reviewed.id} -->`)).toBe(true);

    // The action reported inline_posted=false: the findings move to ui_only,
    // the pass summary becomes the fallback body, and the dossier logs why.
    findingUpdates.length = 0;
    await cp.recordSummaryOnlyPost(reviewed.id);
    expect(findingUpdates).toEqual([
      { id: confirmed.id, state: "ui_only" },
      { id: refuted.id, state: "suppressed_refuted" },
    ]);
    expect(finalizations.at(-1)).toMatchObject({ status: "posted", summaryMd: payload.fallback_summary_md });
    expect(events).toContain("inline_fallback");
  });

});
