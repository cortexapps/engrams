/** The automation interpreter (ADR 0119 D2/D3/D8).
 *
 * One pure walk over a checkpointed definition snapshot. Everything
 * observable happens inside injected steps; waits park on the injected
 * receiver. The registered DBOS workflow body stays thin (snapshot step +
 * this function) — see ADR 0119 for the ENGINE_STEP_CONTRACT rule that
 * governs changes here.
 *
 * Step-contract invariants (bump the contract literal if any change):
 *   - step names: step:<framePath>:<attempt>, plus __snapshot__/__finalize__,
 *     .__cond__ for branch conditions, [i].__until__ for loop exits,
 *     :clock:<n> micro-steps during waits;
 *   - condition/branch/loop decisions are step outputs, never re-evaluated;
 *   - one recv loop, messages for other blocks buffer in arrival order;
 *   - finalize runs exactly once, from every exit path.
 */

import { evaluateFilter, parseFilterGroup } from "./conditions.ts";
import { buildRunContext, recordStepOutputs, type RunContext, type RunSnapshot } from "./context.ts";
import type { EngineDeps } from "./deps.ts";
import type { BlockDef, RetryPolicy } from "./definition.ts";
import { AUTOMATION_TOPIC, type AutomationInbox } from "./inbox.ts";
import { getBlock, type BlockExecutor, type BlockOutcome } from "./blocks/registry.ts";
import { registerEngineBlocks } from "./blocks/index.ts";
import {
  clockStepName,
  conditionStepName,
  framePath,
  stepName,
  untilStepName,
  FINALIZE_STEP,
  SNAPSHOT_STEP,
  type Frame,
} from "./step-name.ts";

/** Single source of truth for terminal run statuses. Everything that gates
 * on terminality (e.g. claimCronOccurrence) derives from this array, so a
 * new status is a compile-time update, never a silently-frozen scheduler. */
export const RUN_TERMINAL_STATUSES = [
  "completed",
  "filtered",
  "failed",
  "superseded",
  "halted",
  "deadline",
] as const;

export type RunTerminalStatus = (typeof RUN_TERMINAL_STATUSES)[number];

export interface EngineRunInput {
  runId: string;
  automationId: string;
  /** The registered workflow body's ENGINE_STEP_CONTRACT literal (ADR 0119
   * D2); carried for observability, never branched on. */
  contract?: number;
}

export interface EngineRunResult {
  status: RunTerminalStatus;
  error?: string;
}

/** Raised inside the walk to reach finalize with a terminal status. */
class RunEnd extends Error {
  constructor(
    readonly status: RunTerminalStatus,
    readonly reason?: string,
  ) {
    super(reason ?? status);
    this.name = "RunEnd";
  }
}

const DEFAULT_RETRY: Required<Pick<RetryPolicy, "attempts">> & Pick<RetryPolicy, "retryOn"> = {
  attempts: 1,
  retryOn: "never",
};

function retryPolicy(block: BlockDef): { attempts: number; retryOn: "transient" | "always" | "never" } {
  const policy = block.retry ?? DEFAULT_RETRY;
  return { attempts: policy.attempts, retryOn: policy.retryOn ?? "transient" };
}

function shouldRetry(outcome: Extract<BlockOutcome, { kind: "error" }>, retryOn: string): boolean {
  if (retryOn === "never") return false;
  if (retryOn === "always") return true;
  return outcome.retryable;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

interface WaitState {
  buffer: AutomationInbox[];
  clockSteps: number;
}

export async function interpretAutomation(
  input: EngineRunInput,
  deps: EngineDeps,
): Promise<EngineRunResult> {
  registerEngineBlocks();

  // step:__snapshot__:0 — pin the definition + inputs and mark the run
  // running. Replay walks exactly this object.
  const snapshot: RunSnapshot = await deps.step(async () => {
    const loaded = await deps.store.loadSnapshot(input.runId);
    await deps.store.markRunning(input.runId, loaded.startedAtMs);
    return loaded;
  }, SNAPSHOT_STEP);

  const ctx = buildRunContext(input.runId, snapshot, deps);
  const wait: WaitState = { buffer: [], clockSteps: 0 };
  const runDeadlineAtMs =
    snapshot.definition.settings.runDeadlineSeconds !== undefined
      ? snapshot.startedAtMs + snapshot.definition.settings.runDeadlineSeconds * 1000
      : null;
  let nowMs = snapshot.startedAtMs;

  const checkpointClock = async (frames: readonly Frame[]): Promise<number> => {
    wait.clockSteps += 1;
    nowMs = await deps.step(async () => deps.clock.nowMs(), clockStepName(frames, wait.clockSteps));
    return nowMs;
  };

  /** One recv with global message handling; returns the matched outputs or
   * null on deadline. */
  const waitForMessage = async (
    frames: readonly Frame[],
    executor: BlockExecutor<never>,
    config: never,
    blockDeadlineS: number | null,
  ): Promise<Record<string, unknown> | null> => {
    const deadlineAtMs =
      blockDeadlineS !== null
        ? Math.min(nowMs + blockDeadlineS * 1000, runDeadlineAtMs ?? Number.POSITIVE_INFINITY)
        : (runDeadlineAtMs ?? null);

    const tryMatch = (msg: AutomationInbox): Record<string, unknown> | "ignore" | "buffer" => {
      if (msg.kind === "stop") throw new RunEnd("halted", msg.reason ?? "stopped");
      if (msg.kind === "supersede") throw new RunEnd("superseded", `superseded by ${msg.byRunId}`);
      const matched = executor.wait!.matches(msg, config, ctx);
      if (matched === null) return "buffer";
      if (matched === "ignore") return "ignore";
      return matched;
    };

    // Drain buffered messages first (deterministic: pure function of the
    // checkpointed recv history).
    for (let i = 0; i < wait.buffer.length; i += 1) {
      const result = tryMatch(wait.buffer[i]!);
      if (result === "buffer") continue;
      wait.buffer.splice(i, 1);
      if (result === "ignore") {
        i -= 1;
        continue;
      }
      return result;
    }

    for (;;) {
      const remainingS =
        deadlineAtMs === null
          ? 3600
          : Math.max(0, Math.ceil((deadlineAtMs - nowMs) / 1000));
      if (deadlineAtMs !== null && remainingS === 0) {
        if (runDeadlineAtMs !== null && deadlineAtMs >= runDeadlineAtMs) {
          throw new RunEnd("deadline", "run deadline expired");
        }
        return null;
      }
      const msg = await deps.recv(AUTOMATION_TOPIC, Math.min(remainingS, 3600));
      if (msg === null) {
        await checkpointClock(frames);
        if (deadlineAtMs === null) continue;
        if (nowMs >= deadlineAtMs) {
          if (runDeadlineAtMs !== null && deadlineAtMs >= runDeadlineAtMs) {
            throw new RunEnd("deadline", "run deadline expired");
          }
          return null;
        }
        continue;
      }
      const result = tryMatch(msg);
      if (result === "buffer") {
        wait.buffer.push(msg);
        await checkpointClock(frames);
        continue;
      }
      if (result === "ignore") {
        await checkpointClock(frames);
        continue;
      }
      return result;
    }
  };

  const runConditionStep = async (
    frames: readonly Frame[],
    name: string,
    raw: unknown,
  ): Promise<boolean> =>
    deps.step(async () => {
      const group = parseFilterGroup(raw);
      return evaluateFilter(group, ctx.scope());
    }, name);

  const runBlock = async (block: BlockDef, frames: Frame[]): Promise<void> => {
    const path = framePath(frames);
    const executor = getBlock(block.type);
    if (!executor) throw new RunEnd("failed", `unknown block type "${block.type}"`);

    // Control blocks: decisions are their own checkpointed steps.
    if (block.type === "filter") {
      const pass = await runConditionStep(
        frames,
        stepName(frames, 0),
        (block.config as { conditions: unknown }).conditions,
      );
      await deps.step(async () => {
        await deps.store.recordStep(input.runId, path, 0, {
          status: pass ? "succeeded" : "skipped",
          outputs: { pass },
        });
      }, `${stepName(frames, 0)}:ledger`);
      if (!pass) throw new RunEnd("filtered", `filter "${block.id}" did not match`);
      return;
    }
    if (block.type === "branch") {
      const taken = await runConditionStep(
        frames,
        conditionStepName(frames),
        (block.config as { conditions: unknown }).conditions,
      );
      recordStepOutputs(ctx, block.id, path, { taken: taken ? "then" : "else" });
      const list = taken ? (block.then ?? []) : (block.else ?? []);
      for (const child of list) await runBlock(child, [...frames, { blockId: child.id }]);
      return;
    }
    if (block.type === "loop") {
      const config = block.config as { until?: unknown; maxIterations: number };
      let iterations = 0;
      let exhausted = true;
      for (let i = 0; i < config.maxIterations; i += 1) {
        const iterationFrames: Frame[] = [...frames.slice(0, -1), { blockId: block.id, iteration: i }];
        for (const child of block.body ?? []) {
          await runBlock(child, [...iterationFrames, { blockId: child.id }]);
        }
        iterations = i + 1;
        if (config.until !== undefined) {
          const done = await runConditionStep(frames, untilStepName(frames, i), config.until);
          if (done) {
            exhausted = false;
            break;
          }
        }
      }
      recordStepOutputs(ctx, block.id, path, { iterations, exhausted });
      return;
    }

    // Data / wait blocks: engine-level retry loop, one step per attempt.
    const policy = retryPolicy(block);
    ctx.currentBlockId = block.id;
    let lastError: Extract<BlockOutcome, { kind: "error" }> | null = null;
    for (let attempt = 0; attempt < policy.attempts; attempt += 1) {
      ctx.currentAttempt = attempt;
      const name = stepName(frames, attempt);
      const outcome: BlockOutcome = await deps.step(async () => {
        await deps.store.recordStep(input.runId, path, attempt, {
          status: "running",
          inputs: block.config,
        });
        let result: BlockOutcome;
        try {
          result = executor.execute
            ? await executor.execute(block.config as never, ctx)
            : { kind: "ok", outputs: {} };
        } catch (error) {
          result = {
            kind: "error",
            code: "block_threw",
            message: errorMessage(error),
            retryable: false,
          };
        }
        await deps.store.recordStep(input.runId, path, attempt, {
          status: result.kind === "error" ? "failed" : "succeeded",
          ...(result.kind === "ok" ? { outputs: result.outputs } : {}),
          ...(result.kind === "error" ? { error: `${result.code}: ${result.message}` } : {}),
        });
        return result;
      }, name);

      if (outcome.kind === "end_run") {
        throw new RunEnd(outcome.status, outcome.reason);
      }
      if (outcome.kind === "error") {
        lastError = outcome;
        if (attempt + 1 < policy.attempts && shouldRetry(outcome, policy.retryOn)) continue;
        throw new RunEnd("failed", `block "${block.id}": ${outcome.code}: ${outcome.message}`);
      }

      recordStepOutputs(ctx, block.id, path, outcome.outputs);
      lastError = null;

      // Wait half, when the block has one and its config asks for it.
      if (executor.wait) {
        const deadlineS = executor.wait.deadlineSeconds(block.config as never, ctx);
        if (deadlineS !== 0) {
          const matched = await waitForMessage(frames, executor, block.config as never, deadlineS);
          const waited =
            matched ??
            (executor.wait.onDeadline
              ? executor.wait.onDeadline(block.config as never, ctx)
              : { outcome: "deadline" });
          const merged = { ...ctx.steps[block.id], ...waited };
          recordStepOutputs(ctx, block.id, path, merged);
          await deps.step(async () => {
            await deps.store.recordStep(input.runId, path, attempt, {
              status: matched === null ? "failed" : "succeeded",
              outputs: merged,
              ...(matched === null ? { error: "deadline" } : {}),
            });
          }, `${name}:wait`);
          if (matched === null) {
            throw new RunEnd("deadline", `block "${block.id}" wait deadline expired`);
          }
          if (merged["outcome"] === "failed") {
            throw new RunEnd("failed", `block "${block.id}": session run failed`);
          }
        }
      }
      break;
    }
    ctx.currentBlockId = undefined;
    ctx.currentAttempt = undefined;
    if (lastError) {
      throw new RunEnd("failed", `block "${block.id}": ${lastError.code}: ${lastError.message}`);
    }
  };

  let terminal: EngineRunResult;
  try {
    for (const block of snapshot.definition.blocks) {
      await runBlock(block, [{ blockId: block.id }]);
    }
    terminal = { status: "completed" };
  } catch (error) {
    if (error instanceof RunEnd) {
      terminal = { status: error.status, ...(error.reason ? { error: error.reason } : {}) };
    } else {
      terminal = { status: "failed", error: errorMessage(error) };
    }
  }

  // step:__finalize__:0 — exactly once, from every exit path. Sessions with
  // keep=false end here (keep defaults per D8: keepOnFinish on the block,
  // else the inverse of end_sessions_on_finish).
  await deps.step(async () => {
    const sessions = await deps.store.listRunSessions(input.runId);
    for (const session of sessions) {
      if (session.keep) continue;
      try {
        await deps.sessions.endSession(session.sessionId);
      } catch {
        // Teardown is best-effort; the reconciler owns stragglers.
      }
    }
    // Terminal status FIRST: promotion selects pending runs, and a run that
    // released its claim must already read as terminal so no later release
    // can promote it.
    await deps.store.finalizeRun(input.runId, terminal.status, terminal.error);
    if (terminal.status !== "superseded") {
      const promoted = await deps.store.releaseConcurrency(input.runId);
      if (promoted !== null && deps.startQueuedRun) await deps.startQueuedRun(promoted);
    }
  }, FINALIZE_STEP);

  return terminal;
}
