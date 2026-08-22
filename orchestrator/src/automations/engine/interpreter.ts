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
 *   - session messages are TURN-CORRELATED: the run counts the prompts it
 *     sent per session (create_session's initial prompt is turn 1); the Nth
 *     session_idle corresponds to the Nth prompt, and an idle or signal from
 *     a turn older than the session's latest prompt is stale — dropped, never
 *     matched. A banked idle from an un-awaited turn can therefore never
 *     satisfy a later wait on the same session. (A human prompting a
 *     run-owned session mid-run inflates the idle count and turns a would-be
 *     false match into a wait deadline — the safe failure.)
 *   - finalize hooks (contract 2): for a run ending in a hook's `when`, the
 *     hook block runs as `step:__finalize__.<blockId>:0` BEFORE the finalize
 *     step, in definition order; outcomes never change the terminal status;
 *   - installed message handlers (contract 3): a block whose executor has
 *     `onMessage` is INSTALLED once its execute step succeeds; from then until
 *     the run ends, every received mailbox message is offered to it first —
 *     inside `step:<relayPath>.__relay__:<n>` (n = per-relay counter, a pure
 *     function of the recv sequence) — BEFORE stop/supersede handling and the
 *     active wait's matcher. "consumed" swallows the message. At most one
 *     installed handler per run (validation);
 *   - finalize runs exactly once, from every exit path.
 */

import { evaluateFilter, parseFilterGroup } from "./conditions.ts";
import { isSafePath, ownPath } from "../paths.ts";
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
  relayStepName,
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

/** `{ "$ref": "steps.x.y" }` → the JSON value at that scope path. Liquid can
 * only produce strings; structured values from earlier blocks (an array of
 * review comments, a PR-context object) pass by reference. */
function asValueRef(value: unknown): string | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const keys = Object.keys(value);
  if (keys.length !== 1 || keys[0] !== "$ref") return null;
  const path = (value as { $ref: unknown }).$ref;
  return typeof path === "string" && isSafePath(path) ? path : null;
}

/** Which config fields the interpreter leaves UNRENDERED: a wait block's
 * session reference is resolved by the block (it reads its own outputs), a
 * code block's source is JS not a template, and condition groups are data. */
const UNRENDERED_FIELDS = new Set(["session", "source", "conditions", "until", "waitFor"]);

async function resolveBlockConfig(
  config: Record<string, unknown>,
  ctx: RunContext,
): Promise<Record<string, unknown>> {
  const scope = ctx.scope();
  const walk = async (value: unknown, top: boolean): Promise<unknown> => {
    const ref = asValueRef(value);
    if (ref !== null) return ownPath(scope, ref);
    if (typeof value === "string") {
      return value.includes("${{") ? ctx.render(value) : value;
    }
    if (Array.isArray(value)) return Promise.all(value.map((v) => walk(v, false)));
    if (typeof value === "object" && value !== null) {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value)) {
        out[k] = top && UNRENDERED_FIELDS.has(k) ? v : await walk(v, false);
      }
      return out;
    }
    return value;
  };
  return (await walk(config, true)) as Record<string, unknown>;
}

/** A buffered message plus the turn bookkeeping stamped when it was first
 * received. Both stamps are pure functions of the checkpointed recv history,
 * so replay reproduces them exactly. */
interface BufferedEntry {
  msg: AutomationInbox;
  /** session_idle: the ordinal of this idle for its session (1-based). */
  idleTurn?: number;
  /** signal with a session: the session's prompt count at receipt. */
  signalTurn?: number;
}

interface WaitState {
  buffer: BufferedEntry[];
  clockSteps: number;
}

/** Per-session prompt/idle counters (see the turn-correlation invariant). */
interface TurnLedger {
  /** Prompts sent per session by this run (create_session initial = 1). */
  started: Map<string, number>;
  /** session_idle messages first-received per session. */
  idleSeen: Map<string, number>;
}

export async function interpretAutomation(
  input: EngineRunInput,
  deps: EngineDeps,
): Promise<EngineRunResult> {
  registerEngineBlocks();

  // step:__snapshot__:0 — pin the definition + inputs and mark the run
  // running. Replay walks exactly this object.
  let snapshot: RunSnapshot;
  try {
    snapshot = await deps.step(async () => {
      const loaded = await deps.store.loadSnapshot(input.runId);
      await deps.store.markRunning(input.runId, loaded.startedAtMs);
      return loaded;
    }, SNAPSHOT_STEP);
  } catch (error) {
    // The run never had a definition to walk (stored inputs the pinned
    // schema rejects, a missing version, ...). It still holds the
    // concurrency claim admission took for it, so it must reach the same
    // finalize every other exit does: terminal status, then release (which
    // promotes a queued successor). Without this a queue|join|skip key
    // stays held by a dead run forever. No sessions and no finalize hooks
    // exist yet, so finalize is only the status + the release.
    const message = error instanceof Error ? error.message : String(error);
    await deps.step(async () => {
      await deps.store.finalizeRun(input.runId, "failed", message);
      const promoted = await deps.store.releaseConcurrency(input.runId);
      if (promoted !== null && deps.startQueuedRun) await deps.startQueuedRun(promoted);
    }, FINALIZE_STEP);
    return { status: "failed", error: message };
  }

  const ctx = buildRunContext(input.runId, snapshot, deps);
  const wait: WaitState = { buffer: [], clockSteps: 0 };
  const ledger: TurnLedger = { started: new Map(), idleSeen: new Map() };
  /** Sessions an end_session block already ended (see finalize). */
  const endedByBlock = new Set<string>();
  /** The one installed message handler (contract 3), once its block ran. */
  let installed:
    | { path: string; executor: BlockExecutor<never>; config: never; count: number }
    | null = null;

  /** Offer a fresh message to the installed handler (if any) inside its own
   * checkpointed step. Returns true when the handler consumed it. */
  const offerToInstalled = async (msg: AutomationInbox): Promise<boolean> => {
    if (installed === null) return false;
    const relay = installed;
    relay.count += 1;
    const verdict = await deps.step(async () => {
      try {
        return await relay.executor.onMessage!(msg, relay.config, ctx);
      } catch (error) {
        // A relay's delivery failure is recorded on its step and never fails
        // the run (the legacy thread loop's "drop it, keep the thread alive").
        await deps.store.recordStep(input.runId, `${relay.path}.__relay__`, relay.count, {
          status: "failed",
          error: errorMessage(error),
        });
        return "pass" as const;
      }
    }, relayStepName(relay.path, relay.count));
    return verdict === "consumed";
  };

  /** Stamp a freshly received message with its turn bookkeeping. */
  const annotate = (msg: AutomationInbox): BufferedEntry => {
    if (msg.kind === "session_idle") {
      const seen = (ledger.idleSeen.get(msg.sessionId) ?? 0) + 1;
      ledger.idleSeen.set(msg.sessionId, seen);
      return { msg, idleTurn: seen };
    }
    if (msg.kind === "signal" && msg.sessionId !== undefined) {
      return { msg, signalTurn: ledger.started.get(msg.sessionId) ?? 0 };
    }
    return { msg };
  };

  /** Turn-correlation gate: a session message from a turn older than the
   * session's latest prompt is stale and must never match a wait. */
  const gate = (entry: BufferedEntry): "stale" | "pass" => {
    const msg = entry.msg;
    if (msg.kind === "session_idle") {
      const started = ledger.started.get(msg.sessionId) ?? 0;
      if ((entry.idleTurn ?? 0) < started) return "stale";
    }
    if (msg.kind === "signal" && msg.sessionId !== undefined) {
      if ((entry.signalTurn ?? 0) < (ledger.started.get(msg.sessionId) ?? 0)) return "stale";
    }
    return "pass";
  };
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

    const tryMatch = (
      entry: BufferedEntry,
    ): Record<string, unknown> | "ignore" | "buffer" | "stale" => {
      const msg = entry.msg;
      if (msg.kind === "stop") throw new RunEnd("halted", msg.reason ?? "stopped");
      if (msg.kind === "supersede") throw new RunEnd("superseded", `superseded by ${msg.byRunId}`);
      if (gate(entry) === "stale") return "stale";
      const matched = executor.wait!.matches(msg, config, ctx);
      if (matched === null) return "buffer";
      if (matched === "ignore") return "ignore";
      return matched;
    };

    // Drain buffered messages first (deterministic: pure function of the
    // checkpointed recv history). Stale entries are removed for good.
    for (let i = 0; i < wait.buffer.length; i += 1) {
      const result = tryMatch(wait.buffer[i]!);
      if (result === "buffer") continue;
      wait.buffer.splice(i, 1);
      if (result === "ignore" || result === "stale") {
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
      if (await offerToInstalled(msg)) {
        await checkpointClock(frames);
        continue;
      }
      const entry = annotate(msg);
      const result = tryMatch(entry);
      if (result === "buffer") {
        wait.buffer.push(entry);
        await checkpointClock(frames);
        continue;
      }
      if (result === "ignore" || result === "stale") {
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

  /** One data block's resolve → validate → execute → record, as the body of
   * one DBOS step. Shared by the graph walk and the finalize hooks so both
   * checkpoint identically. */
  const executeDataBlock = async (
    block: BlockDef,
    executor: BlockExecutor<never>,
    path: string,
    attempt: number,
  ): Promise<BlockOutcome> => {
    // Resolve the config inside the step (checkpointed): `$ref` value
    // references become the referenced scope value, and templated
    // strings render. Blocks that render their own prompt/command fields
    // see them already rendered (a no-op for a plain string).
    let resolved: Record<string, unknown>;
    try {
      resolved = await resolveBlockConfig(block.config, ctx);
      // Save-time validation skipped `$ref` fields; check the resolved
      // shape against the block's schema before the executor sees it.
      const checked = executor.configSchema.safeParse(resolved);
      if (!checked.success) {
        const issue = checked.error.issues[0];
        throw new Error(
          `resolved config invalid at ${issue ? issue.path.join(".") : "config"}: ${issue ? issue.message : "unknown"}`,
        );
      }
      resolved = checked.data as Record<string, unknown>;
    } catch (error) {
      const failure: BlockOutcome = {
        kind: "error",
        code: "config_render_failed",
        message: errorMessage(error),
        retryable: false,
      };
      await deps.store.recordStep(input.runId, path, attempt, {
        status: "failed",
        inputs: block.config,
        error: `${failure.code}: ${failure.message}`,
      });
      return failure;
    }
    await deps.store.recordStep(input.runId, path, attempt, {
      status: "running",
      inputs: resolved,
    });
    let result: BlockOutcome;
    try {
      result = executor.execute
        ? await executor.execute(resolved as never, ctx)
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
  };

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
    ctx.currentPath = path;
    let lastError: Extract<BlockOutcome, { kind: "error" }> | null = null;
    for (let attempt = 0; attempt < policy.attempts; attempt += 1) {
      ctx.currentAttempt = attempt;
      const name = stepName(frames, attempt);
      const outcome: BlockOutcome = await deps.step(
        () => executeDataBlock(block, executor, path, attempt),
        name,
      );

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

      // Contract 3: a block with an onMessage handler is installed for the
      // rest of the run once its execute step succeeded. Validation allows
      // one per definition, so a second install is an engine invariant.
      if (executor.onMessage) {
        if (installed !== null && installed.executor !== executor) {
          throw new RunEnd("failed", `block "${block.id}": a message handler is already installed`);
        }
        if (installed === null) {
          installed = { path, executor, config: block.config as never, count: 0 };
        } else {
          // The same block re-executed (e.g. a loop body re-pointing a relay
          // at a new turn): refresh its config, keep the counter.
          installed.config = block.config as never;
        }
      }

      // Turn ledger: count the prompts this run sends per session, from
      // checkpointed step outputs only (replay-deterministic).
      const outputSessionId = outcome.outputs["session_id"];
      if (typeof outputSessionId === "string") {
        if (block.type === "create_session" && outcome.outputs["initial_prompt"] !== false) {
          ledger.started.set(outputSessionId, 1);
        } else if (block.type === "send_prompt") {
          ledger.started.set(outputSessionId, (ledger.started.get(outputSessionId) ?? 0) + 1);
        } else if (block.type === "end_session" && outcome.outputs["ended"] === true) {
          // Finalize skips what an explicit end_session already ended
          // (replay-deterministic: read from the checkpointed outputs).
          endedByBlock.add(outputSessionId);
        }
      }

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
    ctx.currentPath = undefined;
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

  // Finalize hooks (contract 2): blocks that observe the terminal status
  // before teardown — each its own step, named under __finalize__ so the
  // ledger groups them with the run's end. A hook can post, clean up, or
  // record; it can NEVER change the outcome (a throwing hook is recorded on
  // its own step row and logged), and it never waits (validation refuses
  // wait-capable blocks). `run.status`/`run.error` are in scope so a comment
  // can say why.
  ctx.terminal = { status: terminal.status, ...(terminal.error ? { error: terminal.error } : {}) };
  for (const hook of snapshot.definition.settings.onFinalize ?? []) {
    if (!hook.when.includes(terminal.status)) continue;
    const block = hook.block;
    const executor = getBlock(block.type);
    const path = `__finalize__.${block.id}`;
    ctx.currentBlockId = block.id;
    ctx.currentPath = path;
    ctx.currentAttempt = 0;
    const outcome: BlockOutcome = await deps.step(async () => {
      if (!executor) {
        return {
          kind: "error",
          code: "unknown_block_type",
          message: `unknown block type "${block.type}"`,
          retryable: false,
        };
      }
      return executeDataBlock(block, executor, path, 0);
    }, `step:${path}:0`);
    ctx.currentBlockId = undefined;
    ctx.currentPath = undefined;
    ctx.currentAttempt = undefined;
    if (outcome.kind === "ok") {
      recordStepOutputs(ctx, block.id, path, outcome.outputs);
    }
    // Any other outcome (error, or an end_run a hook has no business
    // issuing) is already on the step row; the terminal status stands.
  }

  // step:__finalize__:0 — exactly once, from every exit path. Sessions with
  // keep=false end here (keep defaults per D8: keepOnFinish on the block,
  // else the inverse of end_sessions_on_finish).
  await deps.step(async () => {
    const sessions = await deps.store.listRunSessions(input.runId);
    for (const session of sessions) {
      if (session.keep || endedByBlock.has(session.sessionId)) continue;
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
