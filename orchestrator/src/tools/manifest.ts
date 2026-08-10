import { z } from "zod";

import {
  toolSupportsTaskType,
  type NativeBindings,
  type ToolRegistry,
} from "./registry.ts";

export interface ToolManifestEntry {
  name: string;
  description: string;
  inputSchema: object;
  execution: "sync" | "deferred";
  nativeBindings: NativeBindings;
}

/** Compile the model-facing manifest from a registry without side effects. */
export function compileToolManifest(
  registry: ToolRegistry,
  capabilities?: readonly string[],
  taskType?: string,
): ToolManifestEntry[] {
  const granted = capabilities == null ? undefined : new Set(capabilities);
  return registry
    .all()
    .filter((tool) =>
      toolSupportsTaskType(tool, taskType) &&
      (tool.capability == null || granted == null || granted.has(tool.capability))
    )
    .map((tool) => ({
      name: tool.name,
      description: tool.description,
      inputSchema: z.toJSONSchema(tool.input),
      execution: tool.execution,
      nativeBindings: tool.nativeBindings ?? {},
    }));
}
