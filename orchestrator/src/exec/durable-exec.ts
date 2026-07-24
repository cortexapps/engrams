/**
 * The caller-side half of ADR 0103 durable exec: attach-or-start with a
 * stable ticket, drain, and re-attach from exact byte offsets until Exit or
 * deadline.
 *
 * This is deliberately its own module, not part of any workflow: consumers
 * (the review control plane today) should know nothing about transport
 * severance, replay offsets, or backoff — they call `runExec(command,
 * {execId, deadlineMs})` and receive the exit status and output. Everything
 * the durable-exec protocol requires of a caller lives here, once:
 *
 * - **Identity.** The `execId` ticket is minted by the caller and stable
 *   across DBOS step replay, so a re-run attaches instead of re-spawning.
 * - **Resumption.** Delivered bytes advance replay offsets; a re-attach asks
 *   for exactly the tail.
 * - **The terminal taxonomy (normative, in the ADR).** Refusals and protocol
 *   violations are deterministic → terminal on first occurrence. Transport
 *   loss retries with capped backoff; the deadline is the ONLY budget —
 *   every attempt-counting scheme tried before was either dead code (the
 *   coordinator prepends ExecStarted on every answered attach, so frames
 *   are not progress) or a hair trigger against silent execs (a non-tty
 *   `git clone` prints nothing until it finishes).
 */

import { Code, ConnectError } from "@connectrpc/connect";

import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "durable-exec" });

export interface ExecOutputFrame {
  event:
    | { case: "started"; value: { execId: string } }
    | { case: "stdout"; value: Uint8Array }
    | { case: "stderr"; value: Uint8Array }
    | { case: "exit"; value: { exitStatus?: number | null } }
    | { case: undefined; value?: undefined };
}

export interface ExecAttachRequest {
  sessionId: string;
  command: string;
  execId?: string;
  stdoutOffset?: bigint;
  stderrOffset?: bigint;
  wake?: boolean;
}

/** The minimal client surface the durable-exec loop needs. */
export interface DurableExecClient {
  exec(
    req: ExecAttachRequest,
    options?: { signal?: AbortSignal },
  ): AsyncIterable<ExecOutputFrame>;
  cancelExec(req: { sessionId: string; execId: string }): Promise<unknown>;
}

export interface RunExecOptions {
  /** Stable across DBOS step replay for exactly-once spawn. */
  execId?: string;
  deadlineMs: number;
}

export interface RunExecResult {
  exitStatus: number | null | undefined;
  stdout: string;
  stderr: string;
}

export interface RunExecRuntime {
  nowMs(): number;
  sleep(ms: number): Promise<void>;
  /** Arm the wall-clock deadline and return a disposer. */
  scheduleDeadline(delayMs: number, onDeadline: () => void): () => void;
}

export const defaultRunExecRuntime: RunExecRuntime = {
  nowMs: Date.now,
  sleep: (ms) => Bun.sleep(ms),
  scheduleDeadline(delayMs, onDeadline) {
    const timer = setTimeout(onDeadline, delayMs);
    return () => clearTimeout(timer);
  },
};

export class RunExecError extends Error {
  constructor(
    message: string,
    readonly stdout: string,
    readonly stderr: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "RunExecError";
  }
}

type AttemptFailure =
  | { kind: "ended" }
  | { kind: "error"; error: unknown }
  | { kind: "protocol"; message: string };

// The no-progress streak shapes BACKOFF only ("progress" = replay offsets
// advanced): it grows toward the cap while nothing lands and resets on bytes
// so healing re-attaches fast. It never gives up — the deadline does.
const EXEC_REATTACH_BASE_DELAY_MS = 50;
const EXEC_REATTACH_MAX_DELAY_MS = 1_000;

function decodeExecOutput(chunks: readonly Uint8Array[]): string {
  let byteLength = 0;
  for (const chunk of chunks) byteLength += chunk.byteLength;
  const joined = new Uint8Array(byteLength);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(joined);
}

function isTerminalExecError(error: unknown): boolean {
  if (!(error instanceof ConnectError)) return false;
  if (error.code === Code.NotFound) return true;
  if (error.code === Code.FailedPrecondition) {
    // The coordinator maps BOTH permanent and self-healing states onto
    // FailedPrecondition: `Gone` (dead/failed/completed session, invalidated
    // snapshot) is permanent, while Evacuating/Queued/Pending conflicts, the
    // transient no-live-sandbox eviction flip, and HostLost are all
    // documented "retry shortly". The engram-error-slug status metadata is
    // the machine-readable discriminator for `Gone`; two deterministic
    // conflict shapes are terminal by message. EVERYTHING else retries —
    // the deadline is the ONLY budget, and if the slug got stripped in
    // transit the failure mode is retry-until-deadline (bounded), never a
    // premature give-up.
    if (error.metadata.get("engram-error-slug") === "snapshot_invalidated") {
      return true;
    }
    // A refusal (first-writer-wins command mismatch, GC'd/missing ticket) is
    // deterministic: retrying cannot change agentd's answer.
    if (/exec refused/i.test(error.rawMessage)) return true;
    // The coordinator's exec-identity invariant guard tripping is equally
    // deterministic — the backend keeps answering with a different ticket.
    if (/durable exec identity changed/i.test(error.rawMessage)) return true;
    return false;
  }
  return ![
    Code.Canceled,
    Code.Unknown,
    Code.DeadlineExceeded,
    Code.Aborted,
    Code.ResourceExhausted,
    Code.Internal,
    Code.Unavailable,
  ].includes(error.code);
}

function reattachDelayMs(consecutiveNoProgressAttempts: number): number {
  return Math.min(
    EXEC_REATTACH_BASE_DELAY_MS *
      2 ** (Math.max(1, consecutiveNoProgressAttempts) - 1),
    EXEC_REATTACH_MAX_DELAY_MS,
  );
}

/** Attach-or-start, drain, and re-attach from exact byte offsets until Exit.
 * This is the sole orchestrator exec caller so step replay and transport
 * severance share one identity/retry implementation. */
export async function runExec(
  sessions: DurableExecClient,
  sessionId: string,
  command: string,
  options: RunExecOptions,
  runtime: RunExecRuntime = defaultRunExecRuntime,
): Promise<RunExecResult> {
  if (!Number.isFinite(options.deadlineMs) || options.deadlineMs <= 0) {
    throw new Error(`exec deadlineMs must be positive, got ${options.deadlineMs}`);
  }

  const stdoutChunks: Uint8Array[] = [];
  const stderrChunks: Uint8Array[] = [];
  let stdoutOffset = 0n;
  let stderrOffset = 0n;
  let canonicalExecId = options.execId;
  let activeAttempt: AbortController | undefined;
  let deadlineFired = false;
  let resolveDeadline!: () => void;
  const deadlineReached = new Promise<void>((resolve) => {
    resolveDeadline = resolve;
  });
  const deadlineAt = runtime.nowMs() + options.deadlineMs;
  const clearDeadline = runtime.scheduleDeadline(options.deadlineMs, () => {
    deadlineFired = true;
    activeAttempt?.abort();
    resolveDeadline();
  });

  const output = () => ({
    stdout: decodeExecOutput(stdoutChunks),
    stderr: decodeExecOutput(stderrChunks),
  });
  // Every give-up path reaps the spawned exec: `CancelExec` exists precisely
  // so a caller that stops reading doesn't leave a command burning guest CPU
  // until journal TTL. No-op until we know the ticket (nothing spawned yet).
  const reap = () => {
    if (canonicalExecId === undefined) return;
    try {
      void sessions.cancelExec({ sessionId, execId: canonicalExecId })
        .catch((error) => {
          log.warn(
            { sessionId, execId: canonicalExecId, error },
            "durable exec cancellation failed (best-effort)",
          );
        });
    } catch (error) {
      log.warn(
        { sessionId, execId: canonicalExecId, error },
        "durable exec cancellation failed (best-effort)",
      );
    }
  };
  const expire = (): never => {
    const execId = canonicalExecId ?? options.execId ?? "<unknown>";
    reap();
    const captured = output();
    throw new RunExecError(
      `exec ${execId} exceeded deadline of ${options.deadlineMs}ms`,
      captured.stdout,
      captured.stderr,
    );
  };
  const deadlineExpired = () =>
    deadlineFired || runtime.nowMs() >= deadlineAt;
  const waitFor = async (promise: Promise<void>): Promise<void> => {
    const outcome = await Promise.race([
      promise.then(() => "ready" as const),
      deadlineReached.then(() => "deadline" as const),
    ]);
    if (outcome === "deadline" || deadlineExpired()) expire();
  };

  let lastFailure: AttemptFailure = { kind: "ended" };
  let noProgressStreak = 0;
  try {
    for (;;) {
      if (deadlineExpired()) expire();

      activeAttempt = new AbortController();
      let iterator: AsyncIterator<ExecOutputFrame> | undefined;
      const attemptStdoutStart = stdoutOffset;
      const attemptStderrStart = stderrOffset;
      try {
        const stream = sessions.exec({
          sessionId,
          command,
          ...(canonicalExecId !== undefined ? { execId: canonicalExecId } : {}),
          stdoutOffset,
          stderrOffset,
          wake: true,
        }, { signal: activeAttempt.signal });
        iterator = stream[Symbol.asyncIterator]();
      } catch (error) {
        lastFailure = { kind: "error", error };
      }

      if (iterator !== undefined) {
        const nextFrame = async (): Promise<
          | { kind: "frame"; value: IteratorResult<ExecOutputFrame> }
          | { kind: "error"; error: unknown }
          | { kind: "deadline" }
        > =>
          Promise.race([
            iterator.next().then(
              (value) => ({ kind: "frame" as const, value }),
              (error: unknown) => ({ kind: "error" as const, error }),
            ),
            deadlineReached.then(() => ({ kind: "deadline" as const })),
          ]);

        const first = await nextFrame();
        if (first.kind === "deadline") return expire();
        if (first.kind === "error") {
          lastFailure = { kind: "error", error: first.error };
        } else if (first.value.done) {
          lastFailure = {
            kind: "protocol",
            message: "stream ended before ExecStarted",
          };
        } else {
          if (first.value.value.event.case !== "started") {
            lastFailure = {
              kind: "protocol",
              message:
                `first frame was ${first.value.value.event.case ?? "empty"}, not ExecStarted`,
            };
          } else {
            const startedExecId = first.value.value.event.value.execId;
            if (startedExecId === "") {
              lastFailure = {
                kind: "protocol",
                message: "ExecStarted carried an empty exec_id",
              };
            } else {
              if (canonicalExecId === undefined) canonicalExecId = startedExecId;
              if (startedExecId !== canonicalExecId) {
                lastFailure = {
                  kind: "protocol",
                  message: `expected ExecStarted{exec_id=${canonicalExecId}}, got ${startedExecId}`,
                };
              } else {
                for (;;) {
                  const next = await nextFrame();
                  if (next.kind === "deadline") return expire();
                  if (next.kind === "error") {
                    lastFailure = { kind: "error", error: next.error };
                    break;
                  }
                  if (next.value.done) {
                    lastFailure = { kind: "ended" };
                    break;
                  }
                  const { event } = next.value.value;
                  if (event.case === "stdout") {
                    stdoutChunks.push(event.value);
                    stdoutOffset += BigInt(event.value.byteLength);
                  } else if (event.case === "stderr") {
                    stderrChunks.push(event.value);
                    stderrOffset += BigInt(event.value.byteLength);
                  } else if (event.case === "exit") {
                    return { exitStatus: event.value.exitStatus, ...output() };
                  } else {
                    lastFailure = {
                      kind: "protocol",
                      message: event.case === "started"
                        ? "received duplicate ExecStarted frame"
                        : "received empty exec frame",
                    };
                    break;
                  }
                }
              }
            }
          }
        }
      }

      activeAttempt.abort();
      activeAttempt = undefined;
      const closeAttempt = iterator?.return?.();
      if (closeAttempt !== undefined) void closeAttempt.catch(() => {});

      if (deadlineExpired()) expire();
      if (
        lastFailure.kind === "error"
        && isTerminalExecError(lastFailure.error)
      ) {
        throw lastFailure.error;
      }
      if (lastFailure.kind === "protocol") {
        // Deterministic contract violation (duplicate/mismatched/empty
        // ExecStarted): terminal on first occurrence — retrying cannot
        // change the answer. Our own exec may be spawned and streaming, so
        // reap it before giving up. (The terminal-error path above must NOT
        // reap: a refusal there means the ticket's real first-writer command
        // is running, and cancelling would kill that legitimate exec; a
        // NotFound / gone-sandbox has nothing to reap.)
        reap();
        const captured = output();
        throw new RunExecError(
          `exec ${canonicalExecId ?? options.execId ?? "<unknown>"} protocol violation: ${lastFailure.message}`,
          captured.stdout,
          captured.stderr,
        );
      }
      if (canonicalExecId === undefined) {
        const captured = output();
        throw new RunExecError(
          "exec attempt failed before ExecStarted; cannot safely retry without a caller-supplied exec_id",
          captured.stdout,
          captured.stderr,
          lastFailure.kind === "error" ? { cause: lastFailure.error } : undefined,
        );
      }
      // Transport loss (retryable error, or a stream that ended cleanly
      // without an Exit): re-attach until the deadline.
      if (
        stdoutOffset > attemptStdoutStart || stderrOffset > attemptStderrStart
      ) {
        noProgressStreak = 0;
      } else {
        noProgressStreak++;
      }

      const delayMs = Math.min(
        reattachDelayMs(noProgressStreak),
        Math.max(0, deadlineAt - runtime.nowMs()),
      );
      await waitFor(runtime.sleep(delayMs));
    }
  } finally {
    activeAttempt?.abort();
    clearDeadline();
  }
}
