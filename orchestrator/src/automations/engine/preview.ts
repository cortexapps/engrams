/** Side-effect-free preview of a definition against a sample (ADR 0119,
 * the editor's "Test with sample").
 *
 * Walks the graph the way the interpreter does — filter/branch/loop
 * decisions through the same condition evaluator — but executes no block:
 * every Liquid template in each block's config is rendered against the scope
 * visible at that block, and the scope itself is reported so the editor's
 * variable picker can show live values. Blocks that would produce outputs at
 * runtime (sessions, commands, actions) contribute nothing to `steps.*` here;
 * a later block that references them renders with those values missing,
 * which the editor surfaces as a render error on that field.
 *
 * The one block that DOES run is `code`: it is pure and sandboxed (QuickJS,
 * frozen JSON in, bounded JSON out, a time budget), so evaluating it is
 * side-effect free, and the built-ins compute admission in a Code block —
 * without its `steps.<id>.value` every filter after it would report a false
 * "fail". Its row shows the value it produced, not its source.
 */

import { evaluateFilter, parseFilterGroup, ConditionParseError } from "./conditions.ts";
import { buildRunContext, type RunContext, type RunSnapshot } from "./context.ts";
import type { AutomationDefinition, BlockDef } from "./definition.ts";
import { entrypointOf, MAIN_ENTRYPOINT_ID } from "./definition.ts";
import type { CodeBlockRuntime, EngineDeps } from "./deps.ts";
import { AutomationTemplateError } from "../template.ts";

export interface PreviewBlockResult {
  blockId: string;
  blockType: string;
  rendered: Record<string, unknown>;
  filterPass?: boolean;
  scope: Record<string, unknown>;
}

export interface PreviewError {
  blockId: string;
  field: string;
  code: string;
  message: string;
}

export interface PreviewResult {
  blocks: PreviewBlockResult[];
  errors: PreviewError[];
}

export interface PreviewInput {
  definition: AutomationDefinition;
  inputs: Record<string, unknown>;
  automationId: string;
  automationName: string;
  trigger: RunSnapshot["trigger"];
  aliases: RunSnapshot["aliases"];
  /** D9: which entrypoint to render (default "main"). */
  entrypointId?: string;
  /** When present, `code` blocks are evaluated (see the module comment). */
  code?: CodeBlockRuntime;
  /** Walk only the admission prelude (see `admissionPrelude`). Dispatch
   * uses this to decide a delivery BEFORE the concurrency claim. */
  preludeOnly?: boolean;
}

/** The admission prelude of a block list: its leading run of `code` and
 * `filter` blocks. Pure and side-effect free, so it can be evaluated before
 * a run exists; the interpreter re-walks it inside the run (same scope,
 * same verdict). A built-in's "facts + admit" pair is the canonical case. */
export function admissionPrelude(blocks: readonly BlockDef[]): BlockDef[] {
  const prelude: BlockDef[] = [];
  for (const block of blocks) {
    if (block.type !== "code" && block.type !== "filter") break;
    prelude.push(block);
  }
  return prelude;
}

/** Deps the preview needs: only `render` goes through the context, and the
 * context needs an EngineDeps shell it never calls. */
function inertDeps(): EngineDeps {
  const unavailable = () => {
    throw new Error("preview never executes");
  };
  return {
    step: unavailable,
    recv: unavailable,
    store: {
      loadSnapshot: unavailable,
      markRunning: unavailable,
      recordStep: unavailable,
      finalizeRun: unavailable,
      listRunSessions: unavailable,
      releaseConcurrency: unavailable,
      adoptSession: unavailable,
      getSessionBinding: unavailable,
    },
    sessions: {
      createSession: unavailable,
      setSessionRelay: unavailable,
      sendPrompt: unavailable,
      endSession: unavailable,
      exec: unavailable,
      writeFiles: unavailable,
      getSession: unavailable,
    },
    clock: { nowMs: () => 0 },
  };
}

async function renderDeep(
  ctx: RunContext,
  value: unknown,
  onError: (field: string, error: unknown) => void,
  field: string,
): Promise<unknown> {
  if (typeof value === "string") {
    if (!value.includes("${{")) return value;
    try {
      return await ctx.render(value);
    } catch (error) {
      onError(field, error);
      return value;
    }
  }
  if (Array.isArray(value)) {
    const out: unknown[] = [];
    for (let i = 0; i < value.length; i += 1) {
      out.push(await renderDeep(ctx, value[i], onError, `${field}[${i}]`));
    }
    return out;
  }
  if (typeof value === "object" && value !== null) {
    const out: Record<string, unknown> = {};
    for (const [key, child] of Object.entries(value)) {
      out[key] = await renderDeep(ctx, child, onError, field === "" ? key : `${field}.${key}`);
    }
    return out;
  }
  return value;
}

function snapshotScope(ctx: RunContext): Record<string, unknown> {
  // A structured clone so later mutation of ctx.steps never leaks backwards.
  return JSON.parse(JSON.stringify(ctx.scope())) as Record<string, unknown>;
}

export async function previewDefinition(input: PreviewInput): Promise<PreviewResult> {
  const snapshot: RunSnapshot = {
    definition: input.definition,
    inputs: input.inputs,
    automationId: input.automationId,
    automationName: input.automationName,
    version: 0,
    trigger: input.trigger,
    aliases: input.aliases,
    startedAtMs: 0,
  };
  const ctx = buildRunContext("preview", snapshot, inertDeps());
  const blocks: PreviewBlockResult[] = [];
  const errors: PreviewError[] = [];

  const errorFor = (blockId: string) => (field: string, error: unknown) => {
    if (error instanceof AutomationTemplateError) {
      errors.push({ blockId, field, code: error.code, message: error.message });
    } else if (error instanceof ConditionParseError) {
      errors.push({ blockId, field, code: "invalid_conditions", message: error.message });
    } else {
      errors.push({
        blockId,
        field,
        code: "render_failed",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const evaluate = (blockId: string, raw: unknown): boolean | null => {
    try {
      return evaluateFilter(parseFilterGroup(raw), ctx.scope());
    } catch (error) {
      errorFor(blockId)("conditions", error);
      return null;
    }
  };

  const walk = async (list: BlockDef[]): Promise<boolean> => {
    for (const block of list) {
      ctx.currentBlockId = block.id;
      const scope = snapshotScope(ctx);
      const rendered = (await renderDeep(ctx, block.config, errorFor(block.id), "")) as Record<
        string,
        unknown
      >;
      const result: PreviewBlockResult = {
        blockId: block.id,
        blockType: block.type,
        rendered,
        scope,
      };
      if (block.type === "code") {
        const config = block.config as { source: string; mode: "value" | "boolean" };
        if (!input.code) {
          blocks.push({ ...result, rendered: { mode: config.mode, value: null } });
          ctx.steps[block.id] = {};
          continue;
        }
        const run = await input.code.evaluate(
          config.source,
          { inputs: ctx.inputs, trigger: ctx.trigger, event: ctx.event, steps: ctx.steps },
          config.mode,
        );
        if (!run.ok) {
          errors.push({
            blockId: block.id,
            field: "source",
            code: `code_${run.error.name.toLowerCase()}`,
            message:
              run.error.line !== undefined
                ? `${run.error.message} (line ${run.error.line})`
                : run.error.message,
          });
          blocks.push({ ...result, rendered: { mode: config.mode, value: null } });
          return false;
        }
        const value = config.mode === "boolean" ? run.value === true : run.value;
        ctx.steps[block.id] = { value };
        blocks.push({
          ...result,
          rendered: { mode: config.mode, value },
          // A boolean-mode code block is a filter: false ends the walk.
          ...(config.mode === "boolean" ? { filterPass: value === true } : {}),
        });
        if (config.mode === "boolean" && value !== true) return false;
        continue;
      }
      if (block.type === "filter") {
        const pass = evaluate(block.id, block.config["conditions"]);
        result.filterPass = pass === true;
        blocks.push(result);
        if (pass !== true) return false;
        continue;
      }
      if (block.type === "branch") {
        const taken = evaluate(block.id, block.config["conditions"]);
        blocks.push(result);
        ctx.steps[block.id] = { taken: taken ? "then" : "else" };
        const next = taken ? (block.then ?? []) : (block.else ?? []);
        if (!(await walk(next))) return false;
        continue;
      }
      if (block.type === "loop") {
        blocks.push(result);
        // One representative iteration: the body renders against iteration 0.
        ctx.loop = { index: 0 };
        const ok = await walk(block.body ?? []);
        ctx.loop = undefined;
        ctx.steps[block.id] = { iterations: 1, exhausted: false };
        if (!ok) return false;
        continue;
      }
      blocks.push(result);
      // Executing blocks leave no outputs in a preview; the picker still shows
      // the block exists so downstream references are discoverable.
      ctx.steps[block.id] = ctx.steps[block.id] ?? {};
    }
    return true;
  };

  const entrypoint = entrypointOf(input.definition, input.entrypointId ?? MAIN_ENTRYPOINT_ID);
  if (entrypoint === null) {
    errors.push({
      blockId: "",
      field: "entrypoint",
      code: "unknown_entrypoint",
      message: `entrypoint "${input.entrypointId}" is not in this definition`,
    });
  } else {
    await walk(input.preludeOnly ? admissionPrelude(entrypoint.blocks) : entrypoint.blocks);
  }
  ctx.currentBlockId = undefined;
  return { blocks, errors };
}
