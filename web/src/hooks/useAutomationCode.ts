/** Code-block hooks: EvalCode (the editor's Run button) and DryRun (ADR 0119
 * phase 3.5). Kept apart from the editor hooks so the CodeMirror chunk's
 * consumers have one small import. */

import { useMutation } from "@connectrpc/connect-query";

import { dryRun, evalCode } from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import type { EvalCodeResponse } from "@/gen/engram/app/v1/automation_pb";

export type CodeMode = "value" | "boolean";

/** The scope EvalCode receives — the same keys the sandbox exposes. */
export interface EvalScope {
  inputs?: unknown;
  trigger?: unknown;
  event?: unknown;
  steps?: unknown;
}

export type EvalOutcome =
  | { ok: true; value: unknown; logs: string[]; durationMs: number }
  | {
      ok: false;
      error: { name: string; message: string; line?: number };
      logs: string[];
      durationMs: number;
    };

/** Flatten the wire response into one discriminated outcome. A response with
 * neither a value nor an error name is a contract violation; surface it as
 * an error rather than a silent success. */
export function evalOutcome(response: EvalCodeResponse): EvalOutcome {
  const logs = [...response.logs];
  const durationMs = Number(response.durationMs);
  if (response.errorName !== undefined || response.errorMessage !== undefined) {
    return {
      ok: false,
      error: {
        name: response.errorName ?? "Error",
        message: response.errorMessage ?? "",
        ...(response.errorLine !== undefined ? { line: response.errorLine } : {}),
      },
      logs,
      durationMs,
    };
  }
  if (response.valueJson === undefined) {
    return {
      ok: false,
      error: {
        name: "ContractError",
        message: "the sandbox returned neither a value nor an error",
      },
      logs,
      durationMs,
    };
  }
  let value: unknown;
  try {
    value = JSON.parse(response.valueJson);
  } catch {
    value = response.valueJson;
  }
  return { ok: true, value, logs, durationMs };
}

export function useEvalCode() {
  return useMutation(evalCode);
}

export function useDryRun() {
  return useMutation(dryRun);
}

/** Build the EvalCode input_json from whatever scope pieces are known. */
export function evalInputJson(scope: EvalScope): string {
  return JSON.stringify({
    inputs: scope.inputs ?? {},
    trigger: scope.trigger ?? {},
    event: scope.event ?? { raw: {} },
    steps: scope.steps ?? {},
  });
}
