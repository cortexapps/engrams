import { isAbsolute, resolve } from "node:path";
import { RunnerFailure } from "./context.ts";
import { selectMatrix } from "./manifest.ts";

export function parseArgs(args: readonly string[]) {
  if (args.length !== 4)
    throw new RunnerFailure("INVALID_CONTEXT", "arguments");
  const values = new Map<string, string>();
  for (let index = 0; index < args.length; index += 2) {
    const flag = args[index]!;
    const value = args[index + 1]!;
    if (
      !["--context", "--matrix"].includes(flag) ||
      values.has(flag) ||
      value.startsWith("--")
    )
      throw new RunnerFailure("INVALID_CONTEXT", "arguments");
    values.set(flag, value);
  }
  const context = values.get("--context");
  const matrix = values.get("--matrix");
  if (
    !context ||
    !isAbsolute(context) ||
    resolve(context) !== context ||
    !matrix
  )
    throw new RunnerFailure("INVALID_CONTEXT", "arguments");
  return { context, selected: selectMatrix(matrix) };
}
