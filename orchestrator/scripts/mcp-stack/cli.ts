import { isAbsolute, resolve } from "node:path";
import { RunnerFailure } from "./context.ts";
import { FULL_MATRIX, selectedCases, type RunSelection } from "./manifest.ts";

export function parseArgs(args: readonly string[]) {
  if (![2, 4].includes(args.length))
    throw new RunnerFailure("INVALID_CONTEXT", "arguments");
  const values = new Map<string, string>();
  for (let index = 0; index < args.length; index += 2) {
    const flag = args[index]!;
    const value = args[index + 1]!;
    if (
      !["--context", "--matrix", "--stage"].includes(flag) ||
      values.has(flag) ||
      value.startsWith("--")
    )
      throw new RunnerFailure("INVALID_CONTEXT", "arguments");
    values.set(flag, value);
  }
  const context = values.get("--context");
  const matrix = values.get("--matrix");
  const stage = values.get("--stage");
  if (
    (stage !== undefined && stage !== "infrastructure") ||
    (stage !== undefined && matrix !== undefined)
  )
    throw new RunnerFailure("INVALID_CONTEXT", "arguments");
  const selection: RunSelection =
    stage === "infrastructure"
      ? { stage }
      : matrix === undefined
        ? FULL_MATRIX
        : { stage: "matrix", matrix };
  if (!context || !isAbsolute(context) || resolve(context) !== context)
    throw new RunnerFailure("INVALID_CONTEXT", "arguments");
  return { context, selection, selected: selectedCases(selection) };
}
