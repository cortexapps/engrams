/** Engine dependency seams (ADR 0119).
 *
 * Everything the interpreter and the block executors touch is injected, in
 * the exact style of AutomationRunWorkflowDeps: production wiring supplies
 * DBOS + Postgres + the control plane; unit tests supply immediate steps, a
 * scripted receiver, and in-memory fakes. No DBOS import in this module.
 */

import type { AutomationInbox } from "./inbox.ts";
import type { RunSnapshot } from "./context.ts";

/** DBOS.runStep, injected. Retries are an ENGINE loop (attempt-scoped step
 * names), so the runner itself never retries. */
export type EngineStepRunner = <T>(fn: () => Promise<T>, name: string) => Promise<T>;

/** DBOS.recv, injected. null = the timeout elapsed. */
export type EngineReceiver = (
  topic: string,
  timeoutSeconds: number,
) => Promise<AutomationInbox | null>;

export interface EngineStepRecord {
  status: "running" | "succeeded" | "failed" | "skipped";
  inputs?: Record<string, unknown>;
  outputs?: Record<string, unknown>;
  error?: string;
}

/** The run/step ledger. Implemented over Postgres in db/automations.ts; the
 * engine calls it only from inside steps, so writes replay idempotently. */
export interface EngineRunStore {
  loadSnapshot(runId: string): Promise<RunSnapshot>;
  markRunning(runId: string, startedAtMs: number): Promise<void>;
  recordStep(
    runId: string,
    framePath: string,
    attempt: number,
    record: EngineStepRecord,
  ): Promise<void>;
  finalizeRun(
    runId: string,
    status: "completed" | "filtered" | "failed" | "superseded" | "halted" | "deadline",
    error?: string,
  ): Promise<void>;
  /** Sessions the run created, with their keep flags (automation_session). */
  listRunSessions(runId: string): Promise<Array<{ sessionId: string; keep: boolean }>>;
  /** Release this run's concurrency claim; returns the promoted successor run
   * id when the policy is queue and a pending run waits, else null. The whole
   * release+promote is one transaction. */
  releaseConcurrency(runId: string): Promise<string | null>;
  /** D11 adoption: re-bind a kept session's `automation_session` row to
   * this run so waits and relays route to its mailbox. Only a row in the
   * SAME automation whose owning run is TERMINAL transfers — adoption must
   * never steal event routing from a live run ("owner_live"). "foreign"
   * covers a row in another automation and no row at all: the binding row
   * is the ownership boundary. */
  adoptSession(input: {
    runId: string;
    automationId: string;
    sessionId: string;
    /** ADR 0120: the adopting run's workstream ('' = unbound). Adoption
     * never crosses instances — a session another workstream's run created
     * classifies as "foreign". */
    instanceId: string;
  }): Promise<"adopted" | "already_ours" | "owner_live" | "foreign">;
  /** Read-only counterpart for the `session_status` probe. */
  getSessionBinding(
    sessionId: string,
  ): Promise<{
    automationId: string;
    runId: string;
    ownerTerminal: boolean;
    /** The owning run's workstream ('' = unbound). */
    instanceId: string;
  } | null>;
}

export interface EngineCreateSessionResult {
  sessionId: string;
  taskId: string;
}

/** Control-plane operations the session-facing blocks use. Implemented over
 * createSessionForExistingTask / sendPrompt / durable exec / WriteFile. */
export interface EngineSessionOps {
  createSession(input: {
    runId: string;
    blockId: string;
    automationId: string;
    profileId: string;
    prompt: string;
    title: string | null;
    role: string;
    keep: boolean;
    /** Run the session as this engrams user (their credentials + task
     * ownership). Unset = the harness's programmatic org credential. */
    ownerUserId?: string;
    harnessMode?: string;
    harness?: string;
    model?: string;
    modelRouter?: string;
    effort?: string;
    /** Session-policy clamps (ADR 0119 phase 4.3): the built-in review
     * workers run with a fixed capability set, a deny-default network, no
     * profile secrets, and a role system prompt. Pass-through to
     * createSessionForExistingTask. */
    capabilityOverride?: readonly string[];
    networkOverride?: { default: "deny" | "allow"; allowHosts: string[]; allowHostPatterns: string[] };
    dropProfileSecretsAndEnv?: boolean;
    appendSystemPrompt?: string;
  }): Promise<EngineCreateSessionResult>;
  sendPrompt(sessionId: string, promptId: string, text: string, harnessMode?: string): Promise<void>;
  /** Contract 3: a relay block asks for this session's curated events. */
  setSessionRelay(sessionId: string, relay: boolean): Promise<void>;
  /** Read-only probe (`session_status`). Not-found is a value, never an
   * error — a swept-away session is a normal answer for a sweep. */
  getSession(sessionId: string): Promise<
    | { found: false }
    | { found: true; status: string; lastActiveAt: string; lastEventAt: string | null }
  >;
  endSession(sessionId: string): Promise<void>;
  exec(
    sessionId: string,
    command: string,
    options: { execId: string; deadlineMs: number },
  ): Promise<{ exitStatus: number | null | undefined; stdout: string; stderr: string }>;
  writeFiles(
    sessionId: string,
    files: Array<{ path: string; content: string; mode?: number }>,
  ): Promise<Array<{ path: string; ok: boolean; error?: string }>>;
}

/** Phase-2 runtimes arrive through setters; until then the code and
 * integration-action executors return typed unavailable errors. */
export interface CodeBlockRuntime {
  evaluate(
    source: string,
    input: Record<string, unknown>,
    mode: "value" | "boolean",
  ): Promise<
    | { ok: true; value: unknown }
    | { ok: false; error: { name: string; message: string; line?: number } }
  >;
}

export interface IntegrationActionRuntime {
  execute(input: {
    provider: string;
    actionId: string;
    connectionId?: string;
    params: Record<string, unknown>;
    runId: string;
    /** Frame path of the executing block (not the block id): the identity
     * every idempotency key (client id, marker) derives from. */
    stepPath: string;
    /** ADR 0120: the executing run's automation + workstream. When bound
     * ('' = unbound), a successful execution writes the action's declared
     * handles to the instance ledger in the SAME step. Additive — fakes
     * that ignore them stay valid. */
    automationId?: string;
    instanceId?: string;
  }): Promise<Record<string, unknown>>;
}

export interface EngineClock {
  nowMs(): number;
}

/** Automation state (ADR 0119 D10): the per-automation KV. Reads and
 * writes run inside checkpointed steps; CAS misses are typed outcomes the
 * graph branches on, never errors. Implemented in db/automation-state.ts. */
export interface EngineStateEntry {
  key: string;
  value: unknown;
  version: number;
  writer: string;
}

/** A caller-fixable limit violation (key too long, value too big,
 * automation full). Thrown by the store; blocks map it to a non-retryable
 * typed error. */
export class StateLimitError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "StateLimitError";
    this.code = code;
  }
}

export type EngineStateSetResult =
  | { ok: true; version: number }
  | { ok: false; current: EngineStateEntry | null };

export interface EngineStateStore {
  get(automationId: string, key: string): Promise<EngineStateEntry | null>;
  set(
    automationId: string,
    key: string,
    value: unknown,
    opts: { writer: string; expectVersion?: number },
  ): Promise<EngineStateSetResult>;
  delete(
    automationId: string,
    key: string,
    opts: { expectVersion?: number },
  ): Promise<{ ok: true; deleted: boolean } | { ok: false; current: EngineStateEntry }>;
  list(
    automationId: string,
    opts?: { prefix?: string; limit?: number },
  ): Promise<{ entries: EngineStateEntry[]; truncated: boolean }>;
}

/** PR → authoring-session lookup (the pr_ref ledger the link consumer
 * maintains). Read-only; the `lookup_pr_session` block's seam. */
export interface EnginePrRefLookup {
  getByPr(repo: string, prNumber: number): Promise<{
    sessionId: string;
    taskId: string | null;
    headBranch: string;
    url: string;
    title: string;
  } | null>;
}

export interface EngineDeps {
  step: EngineStepRunner;
  recv: EngineReceiver;
  store: EngineRunStore;
  sessions: EngineSessionOps;
  clock: EngineClock;
  /** Queue-policy promotion: start the successor run's workflow. The
   * successor's id is fixed before the call, so a replayed finalize cannot
   * double-start it (DBOS start on an existing id is a no-op). */
  startQueuedRun?(runId: string): Promise<void>;
  state?: EngineStateStore;
  prRefs?: EnginePrRefLookup;
  code?: CodeBlockRuntime;
  integrationActions?: IntegrationActionRuntime;
}
