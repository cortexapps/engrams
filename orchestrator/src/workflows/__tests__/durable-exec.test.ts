import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { ProfileRow } from "../../db/profiles.ts";
import type { ReviewDetail, ReviewRow } from "../../db/reviews.ts";
import type { GithubReviewPoster } from "../../reviews/github-review.ts";
import {
  makeReviewControlPlane,
  ReviewSetupError,
  runExec,
  RunExecError,
  type ReviewControlPlane,
  type ReviewControlPlaneDeps,
  type ReviewSessionsClient,
  type RunExecRuntime,
} from "../review-control-plane.ts";
import { prReviewWorkflowImpl, type StepRunner } from "../pr-review.ts";
import type { ReviewInbox } from "../review-inbox.ts";

type ExecRequest = Parameters<ReviewSessionsClient["exec"]>[0];
type ExecFrame =
  | { event: { case: "started"; value: { execId: string } } }
  | { event: { case: "stdout"; value: Uint8Array } }
  | { event: { case: "stderr"; value: Uint8Array } }
  | { event: { case: "exit"; value: { exitStatus?: number | null } } };
type ReviewStoreStub = NonNullable<ReviewControlPlaneDeps["reviews"]>;

const encoder = new TextEncoder();
const HEAD_SHA = "0123456789abcdef0123456789abcdef01234567";
const BASE_SHA = "abcdef0123456789abcdef0123456789abcdef01";

const review: ReviewRow = {
  id: "review-durable-exec",
  repo: "openai/engrams",
  prNumber: 103,
  taskId: "task-durable-exec",
  headSha: HEAD_SHA,
  baseSha: BASE_SHA,
  trigger: "opened",
  status: "queued",
  githubReviewId: null,
  statusCommentId: null,
  finderSessionId: null,
  verifierSessionId: null,
  summaryMd: null,
  createdAt: new Date("2026-07-22T00:00:00Z"),
  updatedAt: new Date("2026-07-22T00:00:00Z"),
};

const reviewerProfile: ProfileRow = {
  id: "reviewer-profile",
  name: "Reviewer",
  description: "",
  icon: "Bot",
  imageId: "reviewer-image",
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
};

function reviewDetail(status: ReviewRow["status"] = review.status): ReviewDetail {
  return { review: { ...review, status }, findings: [], verdicts: [] };
}

function reviewStore(
  overrides: Partial<ReviewStoreStub> = {},
): ReviewStoreStub {
  return {
    getActiveReviewForPr: async () => review,
    getReview: async () => reviewDetail(),
    createReview: async () => review.id,
    updateReviewStatus: async () => {},
    updateFindingState: async () => {},
    finalizeReview: async () => {},
    setStatusCommentId: async () => {},
    setReviewSessionId: async () => {},
    recordEvent: async () => {},
    ...overrides,
  };
}

function instantRuntime(): RunExecRuntime & { elapsedMs(): number } {
  let now = 0;
  return {
    nowMs: () => now,
    async sleep(ms) {
      now += ms;
    },
    scheduleDeadline: () => () => {},
    elapsedMs: () => now,
  };
}

type Terminal = "exit" | "null-exit" | "missing-exit" | "error";

/**
 * A guest-journal-shaped fake: the first request for a ticket spawns once,
 * every later request attaches to the same bytes/result, and disconnect cuts
 * are absolute offsets into each journal file.
 */
class JournalExecServer implements ReviewSessionsClient {
  readonly execCalls: ExecRequest[] = [];
  readonly cancelCalls: Array<{ sessionId: string; execId: string }> = [];
  readonly writeCalls: Array<
    Parameters<ReviewSessionsClient["writeFiles"]>[0]
  > = [];
  readonly deletedIds: string[] = [];
  readonly spawnCounts = new Map<string, number>();

  #journals = new Map<string, string>();
  #minted = 0;

  constructor(
    readonly stdout: Uint8Array,
    readonly stderr: Uint8Array,
    readonly disconnects: Array<{ stdoutEnd: number; stderrEnd: number }> = [],
    readonly terminal: Terminal = "exit",
  ) {}

  async createSession(): Promise<{ sessionId: string }> {
    return { sessionId: "finder-durable" };
  }

  async deleteSession({ sessionId }: { sessionId: string }): Promise<unknown> {
    this.deletedIds.push(sessionId);
    return {};
  }

  async *exec(req: ExecRequest): AsyncGenerator<ExecFrame> {
    this.execCalls.push(req);
    const execId = req.execId ?? `exec:minted:${++this.#minted}`;
    const recorded = this.#journals.get(execId);
    if (recorded === undefined) {
      this.#journals.set(execId, req.command);
      this.spawnCounts.set(execId, (this.spawnCounts.get(execId) ?? 0) + 1);
    } else if (recorded !== req.command) {
      throw new ConnectError(
        `exec_id ${execId} belongs to a different command`,
        Code.InvalidArgument,
      );
    }

    yield { event: { case: "started", value: { execId } } };

    const stdoutStart = Number(req.stdoutOffset ?? 0n);
    const stderrStart = Number(req.stderrOffset ?? 0n);
    const cut = this.disconnects.shift();
    const stdoutEnd = cut === undefined
      ? this.stdout.byteLength
      : Math.max(stdoutStart, Math.min(cut.stdoutEnd, this.stdout.byteLength));
    const stderrEnd = cut === undefined
      ? this.stderr.byteLength
      : Math.max(stderrStart, Math.min(cut.stderrEnd, this.stderr.byteLength));
    if (stdoutEnd > stdoutStart) {
      yield {
        event: {
          case: "stdout",
          value: this.stdout.slice(stdoutStart, stdoutEnd),
        },
      };
    }
    if (stderrEnd > stderrStart) {
      yield {
        event: {
          case: "stderr",
          value: this.stderr.slice(stderrStart, stderrEnd),
        },
      };
    }
    if (cut !== undefined) return;
    if (this.terminal === "error") {
      throw new ConnectError("coordinator stream severed", Code.Unavailable);
    }
    if (this.terminal === "missing-exit") return;
    yield {
      event: {
        case: "exit",
        value: {
          exitStatus: this.terminal === "null-exit" ? null : 0,
        },
      },
    };
  }

  async cancelExec(req: {
    sessionId: string;
    execId: string;
  }): Promise<unknown> {
    this.cancelCalls.push(req);
    return {};
  }

  async writeFiles(
    req: Parameters<ReviewSessionsClient["writeFiles"]>[0],
  ): Promise<{
    results: Array<{ path: string; ok: boolean; error?: string }>;
  }> {
    this.writeCalls.push(req);
    return {
      results: req.files.map((file) => ({ path: file.path, ok: true })),
    };
  }

  async sendPrompt(): Promise<unknown> {
    return {};
  }
}

function bootstrapInput() {
  return {
    reviewId: review.id,
    repo: review.repo,
    headSha: review.headSha,
  };
}

function bootstrapControlPlane(
  sessions: ReviewSessionsClient,
): ReviewControlPlane {
  return makeReviewControlPlane({
    sessions,
    execRuntime: instantRuntime(),
    reviews: reviewStore(),
  });
}

const githubPoster: GithubReviewPoster = {
  fetchPrHeads: async () => ({ headSha: HEAD_SHA, baseSha: BASE_SHA }),
  alreadyPosted: async () => false,
  upsertStatusComment: async () => ({ commentId: "status-comment" }),
  postReview: async () => ({
    posted: true,
    inlinePosted: true,
    summaryMd: "",
  }),
};

describe("durable orchestrator exec caller", () => {
  test("re-attaches from byte offsets without gaps, duplicates, or a second spawn", async () => {
    const stdout = encoder.encode("stdout:αβγ");
    const stderr = encoder.encode("stderr:δε");
    const execId = "exec:session-1:offset-property";

    // Exercise every byte boundary on each stream independently. UTF-8 code
    // points are deliberately split: offsets are bytes, decoding happens once
    // after the exact byte sequence has been reassembled.
    for (let stdoutCut = 0; stdoutCut <= stdout.byteLength; stdoutCut++) {
      for (let stderrCut = 0; stderrCut <= stderr.byteLength; stderrCut++) {
        const server = new JournalExecServer(
          stdout,
          stderr,
          [{ stdoutEnd: stdoutCut, stderrEnd: stderrCut }],
        );
        const result = await runExec(
          server,
          "session-1",
          "printf output",
          { execId, deadlineMs: 10_000 },
          instantRuntime(),
        );

        expect(result).toEqual({
          exitStatus: 0,
          stdout: "stdout:αβγ",
          stderr: "stderr:δε",
        });
        expect(server.spawnCounts.get(execId)).toBe(1);
        expect(server.execCalls).toHaveLength(2);
        expect(server.execCalls[1]?.stdoutOffset).toBe(BigInt(stdoutCut));
        expect(server.execCalls[1]?.stderrOffset).toBe(BigInt(stderrCut));
      }
    }

    // The cap permits eight severances. Walk one byte at a time across both
    // files to pin attach-not-spawn through the maximum retry depth.
    const manyDisconnects = new JournalExecServer(
      encoder.encode("abcde"),
      encoder.encode("xyz"),
      [
        { stdoutEnd: 1, stderrEnd: 0 },
        { stdoutEnd: 1, stderrEnd: 1 },
        { stdoutEnd: 2, stderrEnd: 1 },
        { stdoutEnd: 2, stderrEnd: 2 },
        { stdoutEnd: 3, stderrEnd: 2 },
        { stdoutEnd: 4, stderrEnd: 2 },
        { stdoutEnd: 4, stderrEnd: 3 },
        { stdoutEnd: 5, stderrEnd: 3 },
      ],
    );
    const result = await runExec(
      manyDisconnects,
      "session-many",
      "printf many",
      { execId: "exec:session-many:reattach", deadlineMs: 10_000 },
      instantRuntime(),
    );
    expect(result).toMatchObject({ stdout: "abcde", stderr: "xyz" });
    expect(manyDisconnects.execCalls).toHaveLength(9);
    expect(manyDisconnects.spawnCounts.get("exec:session-many:reattach")).toBe(1);
  });

  test("deadline expiry throws and issues best-effort CancelExec", async () => {
    const server = new JournalExecServer(
      new Uint8Array(),
      encoder.encode("still running"),
      [],
      "error",
    );
    const runtime = instantRuntime();
    const execId = "exec:session-deadline:build";

    await expect(runExec(
      server,
      "session-deadline",
      "sleep forever",
      { execId, deadlineMs: 60 },
      runtime,
    )).rejects.toThrow(`exec ${execId} exceeded deadline of 60ms`);
    expect(server.cancelCalls).toEqual([
      { sessionId: "session-deadline", execId },
    ]);
    expect(server.spawnCounts.get(execId)).toBe(1);
    expect(runtime.elapsedMs()).toBe(60);
  });

  test("not-found and no-live-sandbox coordinator errors are terminal", async () => {
    for (const error of [
      new ConnectError("session not found", Code.NotFound),
      new ConnectError(
        "session has no live sandbox — create a new session or resume from snapshot",
        Code.FailedPrecondition,
      ),
    ]) {
      let calls = 0;
      const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
      const server: ReviewSessionsClient = {
        createSession: () => base.createSession(),
        deleteSession: (req) => base.deleteSession(req),
        cancelExec: (req) => base.cancelExec(req),
        writeFiles: (req) => base.writeFiles(req),
        sendPrompt: () => base.sendPrompt(),
        exec(): AsyncIterable<ExecFrame> {
          calls++;
          return {
            async *[Symbol.asyncIterator]() {
              throw error;
            },
          };
        },
      };

      await expect(runExec(
        server,
        "gone-session",
        "true",
        { execId: "exec:gone-session:true", deadlineMs: 10_000 },
        instantRuntime(),
      )).rejects.toBe(error);
      expect(calls).toBe(1);
    }
  });

  test("missing or mismatched ExecStarted fails the attempt and never consumes output", async () => {
    for (const firstFrame of ["missing", "mismatched"] as const) {
      let calls = 0;
      const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
      const server: ReviewSessionsClient = {
        createSession: () => base.createSession(),
        deleteSession: (req) => base.deleteSession(req),
        cancelExec: (req) => base.cancelExec(req),
        writeFiles: (req) => base.writeFiles(req),
        sendPrompt: () => base.sendPrompt(),
        exec(req) {
          calls++;
          return {
            async *[Symbol.asyncIterator]() {
              if (firstFrame === "mismatched") {
                yield {
                  event: {
                    case: "started",
                    value: { execId: `${req.execId}:wrong` },
                  },
                } satisfies ExecFrame;
              }
            },
          };
        },
      };

      await expect(runExec(
        server,
        "session-protocol",
        "true",
        { execId: "exec:session-protocol:true", deadlineMs: 10_000 },
        instantRuntime(),
      )).rejects.toBeInstanceOf(RunExecError);
      expect(calls).toBe(9);
    }
  });

  test("finder and verifier reject both absent and null exit statuses with guest stderr", async () => {
    for (const phase of ["finder", "verifier"] as const) {
      for (const terminal of ["missing-exit", "null-exit"] as const) {
        const server = new JournalExecServer(
          new Uint8Array(),
          encoder.encode("guest journal unavailable"),
          [],
          terminal,
        );
        const cp = bootstrapControlPlane(server);
        const promise = phase === "finder"
          ? cp.bootstrapFinderSession("finder-stage-1", bootstrapInput())
          : cp.bootstrapVerifierSession("verifier-stage-1", bootstrapInput());

        await expect(promise).rejects.toBeInstanceOf(ReviewSetupError);
        await expect(promise).rejects.toThrow("guest journal unavailable");
        expect(server.writeCalls).toHaveLength(0);
      }
    }
  });

  test("stage-1 fallback drives the review workflow to a durable failed record", async () => {
    for (const terminal of ["missing-exit", "null-exit"] as const) {
      const server = new JournalExecServer(
        new Uint8Array(),
        encoder.encode("stage-1 fallback"),
        [],
        terminal,
      );
      let persistedStatus: ReviewRow["status"] = "queued";
      const events: string[] = [];
      const removedBindings: string[] = [];
      const sessions = server;
      const cp = makeReviewControlPlane({
        sessions,
        execRuntime: instantRuntime(),
        reviews: reviewStore({
          getActiveReviewForPr: async () => ({ ...review, status: persistedStatus }),
          getReview: async () => reviewDetail(persistedStatus),
          updateReviewStatus: async (_reviewId, status) => {
            persistedStatus = status;
          },
          recordEvent: async (_reviewId, kind) => {
            events.push(kind);
          },
        }),
        profiles: {
          getActive: async () => reviewerProfile,
          getByDesignation: async () => reviewerProfile,
        },
        enrollments: { get: async () => null },
        reviewSessions: {
          record: async () => {},
          find: async () => null,
          remove: async (sessionId) => {
            removedBindings.push(sessionId);
          },
        },
        githubPoster,
        createSessionForExistingTask: async () => ({
          sessionId: "finder-stage-1-flow",
        }),
        registerSessionListener: async () => {},
      });
      const messages: Array<ReviewInbox | null> = [{
        kind: "trigger",
        repo: review.repo,
        prNumber: review.prNumber,
        trigger: review.trigger,
        headSha: review.headSha,
      }];
      const steps: string[] = [];
      const step: StepRunner = async (fn, name) => {
        steps.push(name);
        return fn();
      };

      await prReviewWorkflowImpl({
        controlPlane: cp,
        step,
        workflowId: `workflow-${terminal}`,
        recv: async () => messages.shift() ?? null,
      });

      expect(persistedStatus).toBe("failed");
      expect(events).toContain("failed");
      expect(steps.at(-1)).toBe("failReview");
      expect(steps).not.toContain("sendFinderPrompt");
      expect(removedBindings).toEqual(["finder-stage-1-flow"]);
    }
  });

  test("replaying the whole bootstrap method attaches and returns the original result", async () => {
    const execId = "exec:finder-replay:bootstrap-clone";
    const server = new JournalExecServer(
      encoder.encode("original clone output"),
      new Uint8Array(),
    );
    const cp = bootstrapControlPlane(server);

    await cp.bootstrapFinderSession("finder-replay", bootstrapInput());
    await cp.bootstrapFinderSession("finder-replay", bootstrapInput());

    expect(server.spawnCounts.get(execId)).toBe(1);
    expect(server.execCalls).toHaveLength(2);
    expect(server.execCalls.every((call) => call.execId === execId)).toBe(true);
    expect(server.execCalls.every((call) => call.stdoutOffset === 0n)).toBe(true);
    expect(server.writeCalls).toHaveLength(2);
  });
});
