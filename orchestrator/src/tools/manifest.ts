import { compileMcpManifest, type McpSessionBindingSnapshot } from "../mcp/session-bindings.ts";
import { MCP_LIMITS } from "../mcp/limits.ts";
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

/** Preserve native descriptors and raw remote schemas, with one total wire-size bound. */
export function appendMcpManifest(
  nativeJson: string | undefined,
  snapshot: McpSessionBindingSnapshot,
): ToolManifestEntry[] {
  const native: unknown = nativeJson === undefined ? [] : JSON.parse(nativeJson);
  if (!Array.isArray(native)) throw new Error("invalid native tool manifest");
  const entries: ToolManifestEntry[] = [];
  for (const entry of native) {
    if (
      !entry ||
      typeof entry !== "object" ||
      typeof entry.name !== "string" ||
      typeof entry.description !== "string" ||
      !entry.inputSchema ||
      typeof entry.inputSchema !== "object" ||
      !["sync", "deferred"].includes(entry.execution) ||
      !entry.nativeBindings ||
      typeof entry.nativeBindings !== "object"
    )
      throw new Error("invalid native tool descriptor");
    entries.push(entry);
  }
  entries.push(...compileMcpManifest(snapshot));
  if (
    entries.length > MCP_LIMITS.toolsPerSession ||
    new Set(entries.map((entry) => entry.name)).size !== entries.length ||
    Buffer.byteLength(JSON.stringify(entries)) > MCP_LIMITS.manifestBytes
  )
    throw new Error("session tool manifest exceeds limits or contains duplicate names");
  return structuredClone(entries);
}
