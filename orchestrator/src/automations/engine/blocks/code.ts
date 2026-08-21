/** The code block (ADR 0119 D6). Phase 2 supplies the QuickJS-on-WASM
 * runtime through setCodeBlockRuntime; until then execution returns a typed
 * unavailable error while save-time validation accepts the config shape.
 */

import { z } from "zod";

import { registerBlock } from "./registry.ts";

export const CODE_SOURCE_MAX_CHARS = 64 * 1024;

export const codeConfigSchema = z.object({
  source: z.string().min(1).max(CODE_SOURCE_MAX_CHARS),
  mode: z.enum(["value", "boolean"]),
});
export type CodeConfig = z.infer<typeof codeConfigSchema>;

export function registerCodeBlock(): void {
  registerBlock<CodeConfig>({
    type: "code",
    outputs: ["value"],
    configSchema: codeConfigSchema,
    async execute(config, ctx) {
      const runtime = ctx.deps.code;
      if (!runtime) {
        return {
          kind: "error",
          code: "code_runtime_unavailable",
          message: "the code block runtime is not installed",
          retryable: false,
        };
      }
      const result = await runtime.evaluate(
        config.source,
        {
          inputs: ctx.inputs,
          trigger: ctx.trigger,
          event: ctx.event,
          steps: ctx.steps,
        },
        config.mode,
      );
      if (!result.ok) {
        return {
          kind: "error",
          code: `code_${result.error.name.toLowerCase()}`,
          message:
            result.error.line !== undefined
              ? `${result.error.message} (line ${result.error.line})`
              : result.error.message,
          retryable: false,
        };
      }
      if (config.mode === "boolean") {
        if (result.value === false) {
          return { kind: "end_run", status: "filtered", reason: "code predicate returned false" };
        }
        if (result.value !== true) {
          return {
            kind: "error",
            code: "code_contracterror",
            message: "boolean-mode code must return true or false",
            retryable: false,
          };
        }
        return { kind: "ok", outputs: { value: true } };
      }
      return { kind: "ok", outputs: { value: result.value } };
    },
  });
}
