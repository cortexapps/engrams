import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { ProfileRow } from "../../db/profiles.ts";
import type { ReviewDetail, ReviewRow } from "../../db/reviews.ts";
import type { GithubReviewPoster } from "../../reviews/github-review.ts";
import {
  runExec,
  RunExecError,
  type RunExecRuntime,
} from "../../exec/durable-exec.ts";
import {
  makeReviewControlPlane,
  ReviewSetupError,
  type ReviewControlPlane,
  type ReviewControlPlaneDeps,
  type ReviewSessionsClient,
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
  targetId: "target-1",
  provider: "github",
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
  integrationGrants: [{
    connectionId: "connection-engram",
    operation: "pr_review",
    resourceConstraints: [],
  }],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  secrets: [],
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
    claimTargetId: async () => null,
    upsertTarget: async () => ({ id: "target-1" }),
    beginReviewPass: async () => ({
      kind: "created",
      reviewId: review.id,
      taskId: review.taskId,
    }),
    updateReviewPassContext: async () => true,
    getReview: async () => reviewDetail(),
    listPriorPasses: async () => [],
    updateReviewStatus: async () => true,
    updateFindingState: async () => {},
    finalizeReview: async () => true,
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
  fetchPrContext: async () => ({
    headSha: HEAD_SHA,
    baseSha: BASE_SHA,
    pr: {
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
    },
  }),
  alreadyPosted: async () => false,
  listReviewComments: async () => [],
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

  });

  test("more than eight progressive disconnects complete before the deadline", async () => {
    const stdout = encoder.encode("abcdefghijklmn");
    const execId = "exec:session-many:reattach";
    const server = new JournalExecServer(
      stdout,
      new Uint8Array(),
      Array.from(
        { length: 13 },
        (_, index) => ({ stdoutEnd: index + 1, stderrEnd: 0 }),
      ),
    );

    const result = await runExec(
      server,
      "session-many",
      "printf many",
      { execId, deadlineMs: 10_000 },
      instantRuntime(),
    );

    expect(result).toEqual({
      exitStatus: 0,
      stdout: "abcdefghijklmn",
      stderr: "",
    });
    expect(server.execCalls).toHaveLength(14);
    expect(server.spawnCounts.get(execId)).toBe(1);
  });

  test("transport loss retries until the deadline, then cancels", async () => {
    // ADR 0103 terminal taxonomy: the deadline is the ONLY budget for
    // transport loss. No attempt counter may give up early.
    let calls = 0;
    const error = new ConnectError("stream unavailable", Code.Unavailable);
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

    const runtime = instantRuntime();
    await expect(runExec(
      server,
      "session-no-frames",
      "true",
      { execId: "exec:session-no-frames:true", deadlineMs: 60_000 },
      runtime,
    )).rejects.toThrow("exceeded deadline");
    // Backoff caps at 1s, so a 60s outage costs ~60+ attempts — far more
    // than any early-give-up budget, far fewer than a 50ms storm (1200).
    expect(calls).toBeGreaterThan(20);
    expect(calls).toBeLessThan(200);
    expect(base.cancelCalls).toHaveLength(1);
  });

  test("a silent exec survives a transport flap burst and delivers its exit", async () => {
    // The reference consumer is a non-tty `git clone`: it prints NOTHING
    // until it finishes. A ~10s flap burst mid-run must cost retries, never
    // the result (ADR 0103 failure-matrix row 12).
    let calls = 0;
    const execId = "exec:session-silent:clone";
    const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
    const server: ReviewSessionsClient = {
      createSession: () => base.createSession(),
      deleteSession: (req) => base.deleteSession(req),
      cancelExec: (req) => base.cancelExec(req),
      writeFiles: (req) => base.writeFiles(req),
      sendPrompt: () => base.sendPrompt(),
      exec(): AsyncIterable<ExecFrame> {
        const call = ++calls;
        return {
          async *[Symbol.asyncIterator]() {
            yield { event: { case: "started", value: { execId } } };
            if (call <= 12) {
              throw new ConnectError(
                "transport flap mid-silent-clone",
                Code.Unavailable,
              );
            }
            yield { event: { case: "exit", value: { exitStatus: 0 } } };
          },
        };
      },
    };

    const result = await runExec(
      server,
      "session-silent",
      "git clone https://github.com/org/repo /workspace",
      { execId, deadlineMs: 5 * 60_000 },
      instantRuntime(),
    );
    expect(result).toEqual({ exitStatus: 0, stdout: "", stderr: "" });
    expect(calls).toBe(13);
  });

  test("a start-then-fail loop backs off instead of storming, until the deadline", async () => {
    // The coordinator's gRPC handler prepends Started{exec_id} on EVERY
    // attach, so a bare Started is not progress. Without a give-up budget,
    // the bound is the deadline — but the backoff must grow to its cap, not
    // hammer at the 50ms floor (~12000 attempts for this deadline).
    let calls = 0;
    const execId = "exec:session-started-only:true";
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
            yield { event: { case: "started", value: { execId } } };
            throw new ConnectError(
              "stream severed after started",
              Code.Unavailable,
            );
          },
        };
      },
    };

    const runtime = instantRuntime();
    await expect(runExec(
      server,
      "session-started-only",
      "true",
      { execId, deadlineMs: 600_000 },
      runtime,
    )).rejects.toThrow("exceeded deadline");
    expect(calls).toBeGreaterThan(100);
    expect(calls).toBeLessThan(1_000);
  });

  test("a protocol violation is terminal on the first occurrence", async () => {
    // A duplicate ExecStarted is deterministic — retrying cannot change it.
    let calls = 0;
    const execId = "exec:session-proto:true";
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
            yield { event: { case: "started", value: { execId } } };
            yield { event: { case: "started", value: { execId } } };
          },
        };
      },
    };

    await expect(runExec(
      server,
      "session-proto",
      "true",
      { execId, deadlineMs: 600_000 },
      instantRuntime(),
    )).rejects.toThrow("received duplicate ExecStarted frame");
    expect(calls).toBe(1);
    // Our own exec spawned and is streaming; giving up must reap it so it
    // doesn't burn guest CPU until journal TTL.
    expect(base.cancelCalls).toEqual([{ sessionId: "session-proto", execId }]);
  });

  test("an exec refusal is terminal on the first occurrence", async () => {
    // ADR 0103 terminal taxonomy: a refusal (first-writer-wins mismatch,
    // GC'd ticket) is deterministic and must fail fast, never retry.
    let calls = 0;
    const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
    const refusal = new ConnectError(
      "exec refused: exec_id exec:x already belongs to command [\"other\"]",
      Code.FailedPrecondition,
    );
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
            throw refusal;
          },
        };
      },
    };

    await expect(runExec(
      server,
      "session-refused",
      "true",
      { execId: "exec:x", deadlineMs: 600_000 },
      instantRuntime(),
    )).rejects.toThrow("exec refused");
    expect(calls).toBe(1);
    // A refusal must NOT reap: the ticket's real first-writer command is
    // running, and cancelling by this exec_id would kill that legitimate
    // exec.
    expect(base.cancelCalls).toHaveLength(0);
  });

  test("a mid-output stream error re-attaches for the tail and one real exit", async () => {
    let calls = 0;
    let exitFrames = 0;
    const execId = "exec:session-stream-error:build";
    const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
    const server: ReviewSessionsClient = {
      createSession: () => base.createSession(),
      deleteSession: (req) => base.deleteSession(req),
      cancelExec: (req) => base.cancelExec(req),
      writeFiles: (req) => base.writeFiles(req),
      sendPrompt: () => base.sendPrompt(),
      exec(req): AsyncIterable<ExecFrame> {
        const call = ++calls;
        return {
          async *[Symbol.asyncIterator]() {
            yield { event: { case: "started", value: { execId } } };
            if (call === 1) {
              expect(req.stdoutOffset).toBe(0n);
              yield {
                event: { case: "stdout", value: encoder.encode("head-") },
              };
              throw new ConnectError(
                "coordinator transport severed",
                Code.Unavailable,
              );
            }
            expect(req.stdoutOffset).toBe(5n);
            yield {
              event: { case: "stdout", value: encoder.encode("tail") },
            };
            exitFrames++;
            yield {
              event: { case: "exit", value: { exitStatus: 0 } },
            };
          },
        };
      },
    };

    const result = await runExec(
      server,
      "session-stream-error",
      "build",
      { execId, deadlineMs: 10_000 },
      instantRuntime(),
    );

    expect(result).toEqual({
      exitStatus: 0,
      stdout: "head-tail",
      stderr: "",
    });
    expect(calls).toBe(2);
    expect(exitFrames).toBe(1);
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

  test("not-found and gone-session coordinator errors are terminal", async () => {
    for (const error of [
      new ConnectError("session not found", Code.NotFound),
      // `ApiError::Gone` (dead/failed/completed session, invalidated
      // snapshot) shares FailedPrecondition with retryable conflicts; the
      // coordinator's engram-error-slug metadata is the discriminator.
      new ConnectError(
        "session is dead and cannot be resumed",
        Code.FailedPrecondition,
        new Headers({ "engram-error-slug": "snapshot_invalidated" }),
      ),
      // A durable-exec identity change is an invariant guard tripping —
      // deterministic, retrying cannot change the answer.
      new ConnectError(
        "durable exec identity changed across host boundary: requested exec:a, got exec:b",
        Code.FailedPrecondition,
        new Headers({ "engram-error-slug": "conflict" }),
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

  test("documented-retryable conflicts never give up before the deadline", async () => {
    // The coordinator maps Evacuating/Queued/Pending conflicts, the
    // transient no-live-sandbox eviction flip, AND HostLost all to
    // FailedPrecondition — every one of them documented "retry shortly" /
    // self-healing. The deadline is the ONLY budget: none of these may be
    // terminal, whether or not the slug metadata survived the transport.
    const retryable = [
      new ConnectError(
        "session is relocating (operator drain / teleport); it will resume automatically — retry shortly",
        Code.FailedPrecondition,
        new Headers({ "engram-error-slug": "conflict" }),
      ),
      new ConnectError(
        "session has no live sandbox — create a new session or resume from snapshot",
        Code.FailedPrecondition,
      ),
      new ConnectError(
        "host lost; the session will be redriven to Idle",
        Code.FailedPrecondition,
        new Headers({ "engram-error-slug": "host_lost" }),
      ),
    ];
    for (const error of retryable) {
      let calls = 0;
      const base = new JournalExecServer(new Uint8Array(), new Uint8Array());
      const execId = "exec:evacuating:true";
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
        "evacuating-session",
        "true",
        { execId, deadlineMs: 5_000 },
        instantRuntime(),
      )).rejects.toThrow(`exec ${execId} exceeded deadline of 5000ms`);
      // Retried throughout the budget instead of giving up on attempt one.
      expect(calls).toBeGreaterThan(1);
      // The deadline give-up reaps the ticket we may have spawned.
      expect(base.cancelCalls).toEqual([
        { sessionId: "evacuating-session", execId },
      ]);
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
      // Both are deterministic protocol violations: terminal on the first
      // occurrence, never retried.
      expect(calls).toBe(1);
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
          getReview: async () => reviewDetail(persistedStatus),
          updateReviewStatus: async (_reviewId, status) => {
            persistedStatus = status;
            return true;
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
        reviewId: review.id,
        taskId: review.taskId,
        repo: review.repo,
        prNumber: review.prNumber,
        trigger: review.trigger,
        headSha: review.headSha,
        baseSha: review.baseSha,
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
