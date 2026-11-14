import { expect, test } from "bun:test";
import { discoveredCatalog } from "../mcp/discovery.ts";
test("discovery projects only bounded reviewed catalog input and rejects injection", () => {
  const input = {
    name: "lookup",
    inputSchema: { type: "object" },
    annotations: { readOnlyHint: true },
  };
  expect(discoveredCatalog([input])).toEqual([
    { upstreamName: "lookup", description: "", inputSchema: { type: "object" } },
  ]);
  for (const value of [
    [input, input],
    [{ ...input, inputSchema: { $ref: "https://schema.example.test/" } }],
    [{ ...input, name: "bad\nname" }],
    [{ ...input, endpoint: "https://other.example.test/" }],
    [{ ...input, description: "x".repeat(16385) }],
  ])
    expect(() => discoveredCatalog(value)).toThrow();
});
