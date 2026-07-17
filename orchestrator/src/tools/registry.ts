import { z } from "zod";
import { sessions } from "../control-plane/client.ts";
import { makePendingToolCallStore } from "./pending-tool-calls.ts";
import {
  completeRegisteredToolCall,
  type ToolCompletionDeps,
} from "./complete.ts";

export type ToolHandling = "handled" | "session";
export type ToolExecution = "sync" | "deferred";
export type ToolPresenters = Record<string, unknown>;
export interface NativeBindings {
  claude?: string;
  codex?: string;
}

/** The session-scoped half of a tool invocation's context, resolvable
 *  before any specific call exists. */
export interface SessionToolContext {
  sessionId: string;
  capabilities: readonly string[];
  taskId?: string;
  profileId?: string;
  userId?: string;
}

/** Context supplied to an orchestrator-handled tool invocation. Carries the
 *  call identity so a deferred handler can later call
 *  `tools.complete(ctx.sessionId, ctx.toolCallId, result)`. */
export interface ToolContext extends SessionToolContext {
  toolCallId: string;
  toolName: string;
}

/** The sole protocol-level exception to a handled tool's output schema. */
export interface ToolProtocolError {
  error: string;
}

export type ToolHandler<
  TInput extends z.ZodType = z.ZodType,
  TOutput extends z.ZodType = z.ZodType,
> = (
  ctx: ToolContext,
  args: z.output<TInput>,
) =>
  | Promise<z.input<TOutput> | ToolProtocolError | void>
  | z.input<TOutput>
  | ToolProtocolError
  | void;

interface ToolDefinitionBase<
  TInput extends z.ZodType,
  TOutput extends z.ZodType,
> {
  name: string;
  description: string;
  input: TInput;
  output: TOutput;
  presenters?: ToolPresenters;
  presenterExempt?: string;
  nativeBindings?: NativeBindings;
  capability?: string;
}

export interface HandledToolDefinition<
  TInput extends z.ZodType = z.ZodType,
  TOutput extends z.ZodType = z.ZodType,
> extends ToolDefinitionBase<TInput, TOutput> {
  handling: "handled";
  execution: ToolExecution;
  handler: ToolHandler<TInput, TOutput>;
}

export interface SessionToolDefinition<
  TInput extends z.ZodType = z.ZodType,
  TOutput extends z.ZodType = z.ZodType,
> extends ToolDefinitionBase<TInput, TOutput> {
  handling: "session";
  execution?: "deferred";
  handler?: never;
}

export type ToolRegistration<
  TInput extends z.ZodType = z.ZodType,
  TOutput extends z.ZodType = z.ZodType,
> = HandledToolDefinition<TInput, TOutput> | SessionToolDefinition<TInput, TOutput>;

/** Runtime-normalized definition stored by the registry. */
export type RegisteredTool =
  | HandledToolDefinition
  | (Omit<SessionToolDefinition, "execution"> & { execution: "deferred" });

export interface ToolRegistry {
  register<TInput extends z.ZodType, TOutput extends z.ZodType>(
    definition: ToolRegistration<TInput, TOutput>,
  ): RegisteredTool;
  get(name: string): RegisteredTool | undefined;
  all(): readonly RegisteredTool[];
  complete(sessionId: string, toolCallId: string, result: unknown): Promise<void>;
}

export interface ToolRegistryOptions {
  completion?: ToolCompletionDeps | (() => ToolCompletionDeps);
}

class DefaultToolRegistry implements ToolRegistry {
  readonly #byName = new Map<string, RegisteredTool>();
  readonly #completion?: ToolCompletionDeps | (() => ToolCompletionDeps);

  constructor(options: ToolRegistryOptions) {
    this.#completion = options.completion;
  }

  register<TInput extends z.ZodType, TOutput extends z.ZodType>(
    definition: ToolRegistration<TInput, TOutput>,
  ): RegisteredTool {
    if (this.#byName.has(definition.name)) {
      throw new Error(`tool already registered: ${definition.name}`);
    }

    const candidate = definition as ToolRegistration;
    let normalized: RegisteredTool;
    if (candidate.handling === "handled") {
      const handler: unknown = candidate.handler;
      if (typeof handler !== "function") {
        throw new Error(`handled tool ${candidate.name} requires a handler`);
      }
      normalized = candidate;
    } else {
      if (typeof (candidate as { handler?: unknown }).handler === "function") {
        throw new Error(`session tool ${candidate.name} forbids a handler`);
      }
      if (candidate.execution != null && candidate.execution !== "deferred") {
        throw new Error(`session tool ${candidate.name} must use deferred execution`);
      }
      normalized = { ...candidate, execution: "deferred" };
    }

    this.#byName.set(normalized.name, normalized);
    return normalized;
  }

  get(name: string): RegisteredTool | undefined {
    return this.#byName.get(name);
  }

  all(): readonly RegisteredTool[] {
    return [...this.#byName.values()];
  }

  async complete(sessionId: string, toolCallId: string, result: unknown): Promise<void> {
    if (!this.#completion) throw new Error("tool completion is not configured for this registry");
    const deps = typeof this.#completion === "function" ? this.#completion() : this.#completion;
    await completeRegisteredToolCall(this, deps, sessionId, toolCallId, result);
  }
}

export function createToolRegistry(options: ToolRegistryOptions = {}): ToolRegistry {
  return new DefaultToolRegistry(options);
}

/** Process-wide production registry populated by tool modules at startup. */
export const tools: ToolRegistry = createToolRegistry({
  completion: () => ({
    pendingCalls: makePendingToolCallStore(),
    completer: sessions,
    now: () => new Date(),
  }),
});
