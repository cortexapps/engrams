import { canonicalDigest, canonicalJson } from "./hash.ts";
import { MCP_LIMITS } from "./limits.ts";
import type { McpReviewedTool } from "./types.ts";

/** Bound schema traversal before disposable-worker compilation. No remote resolution. */
export function assertSchemaShape(schema: unknown): asserts schema is Record<string, unknown> {
  if (!schema || typeof schema !== "object" || Array.isArray(schema))
    throw new Error("schema must be an object");
  if (Buffer.byteLength(canonicalJson(schema)) > MCP_LIMITS.argumentBytes)
    throw new Error("schema is too large");
  const schemaMaps = new Set([
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
  ]);
  const schemaLists = new Set(["allOf", "anyOf", "oneOf", "prefixItems"]);
  const schemaValues = new Set([
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "not",
    "if",
    "then",
    "else",
    "additionalItems",
    "unevaluatedItems",
    "contentSchema",
  ]);
  let nodes = 0;
  function walk(v: unknown, depth: number, schemaNode: boolean): void {
    if (++nodes > 10_000 || depth > 64) throw new Error("schema is too complex");
    if (!v || typeof v !== "object") return;
    if (Array.isArray(v)) {
      for (const child of v) walk(child, depth + 1, false);
      return;
    }
    for (const [key, child] of Object.entries(v)) {
      if (
        schemaNode &&
        ["$ref", "$dynamicRef", "$recursiveRef"].includes(key) &&
        (typeof child !== "string" || !child.startsWith("#"))
      )
        throw new Error("schema references must be local");
      if (
        schemaNode &&
        key === "$schema" &&
        ![
          "http://json-schema.org/draft-07/schema#",
          "https://json-schema.org/draft/2020-12/schema",
        ].includes(String(child))
      )
        throw new Error("unsupported schema dialect");
      // Schema-map keys are property names. Annotation values are data.
      if (
        schemaNode &&
        (schemaMaps.has(key) || key === "dependencies") &&
        child &&
        typeof child === "object" &&
        !Array.isArray(child)
      ) {
        for (const sub of Object.values(child)) walk(sub, depth + 2, !Array.isArray(sub));
      } else if (schemaNode && (schemaLists.has(key) || key === "items") && Array.isArray(child)) {
        for (const sub of child) walk(sub, depth + 2, true);
      } else walk(child, depth + 1, schemaNode && (schemaValues.has(key) || key === "items"));
    }
  }
  walk(schema, 0, true);
}

export function reviewCatalog(input: readonly unknown[]): McpReviewedTool[] {
  if (input.length > MCP_LIMITS.toolsPerSession) throw new Error("too many tools");
  const names = new Set<string>();
  const tools = input.map((raw) => {
    if (!raw || typeof raw !== "object") throw new Error("invalid reviewed tool");
    const t = raw as Record<string, unknown>;
    if (
      typeof t.upstreamName !== "string" ||
      !t.upstreamName ||
      t.upstreamName.length > 256 ||
      /[\u0000-\u001f]/.test(t.upstreamName) ||
      names.has(t.upstreamName)
    )
      throw new Error("invalid or duplicate tool name");
    names.add(t.upstreamName);
    if (
      typeof t.description !== "string" ||
      t.description.length > 16_384 ||
      !["read", "write"].includes(String(t.classification)) ||
      typeof t.enabled !== "boolean"
    )
      throw new Error("each tool needs explicit review");
    assertSchemaShape(t.inputSchema);
    if (t.outputSchema !== undefined) assertSchemaShape(t.outputSchema);
    const tool = {
      upstreamName: t.upstreamName,
      description: t.description,
      inputSchema: t.inputSchema,
      ...(t.outputSchema === undefined ? {} : { outputSchema: t.outputSchema }),
      classification: t.classification as "read" | "write",
      enabled: t.enabled,
    };
    return structuredClone({ ...tool, fingerprint: canonicalDigest(tool) });
  });
  if (Buffer.byteLength(canonicalJson(tools)) > MCP_LIMITS.manifestBytes)
    throw new Error("catalog is too large");
  return tools;
}
