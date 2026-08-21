/** Block registry (ADR 0119 D1).
 *
 * Executors register at import time (blocks/index.ts); the registry is
 * static, like SWEEP_POLICIES, and `assertBlockRegistryComplete()` runs at
 * boot. `system.*` types are code-registered product logic that only
 * built-in definitions may reference.
 */

import type { z } from "zod";

import type { AutomationInbox } from "../inbox.ts";
import type { RunContext } from "../context.ts";

export type BlockOutcome<O extends Record<string, unknown> = Record<string, unknown>> =
  | { kind: "ok"; outputs: O }
  | { kind: "end_run"; status: "filtered" | "completed"; reason?: string }
  | { kind: "error"; code: string; message: string; retryable: boolean };

export interface BlockWaitSpec<C> {
  /** null = no per-block deadline (the run deadline still applies). */
  deadlineSeconds(config: C, ctx: RunContext): number | null;
  /** Outputs = consume; "ignore" = drop (e.g. duplicate idle); null = not
   * for this block, buffer it. */
  matches(
    msg: AutomationInbox,
    config: C,
    ctx: RunContext,
  ): Record<string, unknown> | "ignore" | null;
  /** Step outputs when the deadline expires (typed outcome, not an error). */
  onDeadline?(config: C, ctx: RunContext): Record<string, unknown>;
}

export interface BlockExecutor<C = unknown> {
  type: string;
  /** Reserved for built-in definitions ("system.*" types). */
  system?: boolean;
  /** Documented for the UI/catalog; not enforced at runtime. */
  outputs?: readonly string[];
  configSchema: z.ZodType<C>;
  /** Data half: runs inside one DBOS step. Absent for pure waits. */
  execute?(config: C, ctx: RunContext): Promise<BlockOutcome>;
  /** Wait half: the interpreter parks on recv and routes messages here. */
  wait?: BlockWaitSpec<C>;
  /** When true the interpreter refuses to run the block without a resolvable
   * session (config carries a SessionRef). */
  requiresSession?: boolean;
}

const registry = new Map<string, BlockExecutor<never>>();

export function isSystemBlockType(type: string): boolean {
  return type.startsWith("system.");
}

export function registerBlock<C>(executor: BlockExecutor<C>): void {
  if (registry.has(executor.type)) {
    throw new Error(`block type "${executor.type}" registered twice`);
  }
  if (isSystemBlockType(executor.type) !== (executor.system === true)) {
    throw new Error(`block type "${executor.type}" must mark system iff its name is system.*`);
  }
  registry.set(executor.type, executor as BlockExecutor<never>);
}

export function getBlock(type: string): BlockExecutor<never> | undefined {
  return registry.get(type);
}

export function listBlockTypes(): string[] {
  return [...registry.keys()].sort();
}

/** Test hook: registries are module-level; tests that register throwaway
 * types clean up after themselves. */
export function unregisterBlockForTest(type: string): void {
  registry.delete(type);
}
