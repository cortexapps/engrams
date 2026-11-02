import { expect, test } from "bun:test";
import { mcpStartupSettings } from "../mcp/runtime-config.ts";
import { makeManagedSessionScanner } from "../mcp/managed-scanner.ts";

test("MCP startup requires explicit global opt-in and retains configured recovery when disabled", () => {
  expect(mcpStartupSettings({})).toEqual({ enabled: false, path: undefined });
  expect(mcpStartupSettings({ MCP_RUNTIME_CONFIG: "/operator/runtime.json" })).toEqual({
    enabled: false,
    path: "/operator/runtime.json",
  });
  expect(
    mcpStartupSettings({ MCP_ENABLED: "true", MCP_RUNTIME_CONFIG: "/operator/runtime.json" })
      .enabled,
  ).toBe(true);
  expect(() => mcpStartupSettings({ MCP_ENABLED: "true" })).toThrow("configuration required");
  expect(() => mcpStartupSettings({ MCP_ENABLED: "1" })).toThrow("true or false");
});

test("managed scanner advances past a failed row and wraps its durable keyset cursor", async () => {
  const reads: (string | undefined)[] = [],
    driven: string[] = [];
  const scanner = makeManagedSessionScanner({
    store: {
      async pending(after) {
        reads.push(after);
        return after === undefined ? [{ sessionId: "a" }, { sessionId: "b" }] : [];
      },
    },
    runtime: {
      async runOnce(id) {
        driven.push(id);
        if (id === "a") throw Error("unavailable");
      },
    },
  });
  await scanner.runOnce();
  await scanner.runOnce();
  await scanner.runOnce();
  expect(reads).toEqual([undefined, "b", undefined]);
  expect(driven).toEqual(["a", "b", "a", "b"]);
  await scanner.stop();
});
