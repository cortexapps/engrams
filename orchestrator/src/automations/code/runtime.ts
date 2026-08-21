/** Adapter from the sandbox to the engine's CodeBlockRuntime seam
 * (ADR 0119 D6). The code block executor owns the outcome mapping (boolean
 * false → filtered; every error non-retryable); this adapter only reshapes.
 */

import type { CodeBlockRuntime } from "../engine/deps.ts";
import { evaluateCode } from "./sandbox.ts";

export function makeCodeBlockRuntime(): CodeBlockRuntime {
  return {
    async evaluate(source, input, mode) {
      const outcome = await evaluateCode(source, input, mode);
      if (outcome.ok) return { ok: true, value: outcome.value };
      return {
        ok: false,
        error: {
          name: outcome.error.name,
          message: outcome.error.message,
          ...(outcome.error.line !== undefined ? { line: outcome.error.line } : {}),
        },
      };
    },
  };
}
