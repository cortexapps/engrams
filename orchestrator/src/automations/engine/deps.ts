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
  }): Promise<Record<string, unknown>>;
}

export interface EngineClock {
  nowMs(): number;
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
  code?: CodeBlockRuntime;
  integrationActions?: IntegrationActionRuntime;
}
