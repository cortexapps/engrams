import { expect, spyOn, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";
import { mcpStartupSettings } from "../mcp/runtime-config.ts";
import { makeManagedSessionScanner } from "../mcp/managed-scanner.ts";
import { log } from "../log.ts";

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

test("managed scanner shutdown waits for its current reconciliation", async () => {
  let release!: () => void, enter!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  const entered = new Promise<void>((resolve) => {
    enter = resolve;
  });
  const driven: string[] = [];
  const scanner = makeManagedSessionScanner({
    store: {
      async pending() {
        return [{ sessionId: "pending" }, { sessionId: "later" }];
      },
    },
    runtime: {
      async runOnce(id) {
        driven.push(id);
        enter();
        await held;
      },
    },
  });
  const started = scanner.start();
  await entered;
  let settled = false;
  const stopped = scanner.stop().then(() => {
    settled = true;
  });
  await Promise.resolve();
  expect(settled).toBe(false);
  release();
  await started;
  await stopped;
  expect(settled).toBe(true);
  expect(driven).toEqual(["pending"]);
});

test.each([
  ["MCP prepared receipt unavailable", "prepared_receipt_unavailable"],
  ["MCP boot publication changed", "boot_publication_changed"],
  ["MCP finalization reservation changed", "finalization_reservation_changed"],
])("managed scanner reports the fixed reason for %s", async (message, reason) => {
  const warning = spyOn(log, "warn").mockImplementation(() => {});
  const reads: (string | undefined)[] = [],
    driven: string[] = [];
  const scanner = makeManagedSessionScanner({
    store: {
      async pending(after) {
        reads.push(after);
        return after === undefined ? [{ sessionId: "pending" }, { sessionId: "later" }] : [];
      },
    },
    runtime: {
      async runOnce(id) {
        driven.push(id);
        if (id === "pending")
          throw new Error(message, { cause: { authorization: "Bearer test-secret" } });
      },
    },
  });
  try {
    await scanner.runOnce();
    await scanner.runOnce();
    await scanner.runOnce();
    expect(reads).toEqual([undefined, "later", undefined]);
    expect(driven).toEqual(["pending", "later", "pending", "later"]);
    expect(warning.mock.calls).toEqual([
      [{ reason }, "MCP managed session reconciliation unavailable"],
      [{ reason }, "MCP managed session reconciliation unavailable"],
    ]);
  } finally {
    warning.mockRestore();
  }
});

test.each([
  new Error("Authorization: Bearer test-secret https://example.test/?token=test-secret"),
  new Error("MCP prepared receipt unavailable: Bearer test-secret"),
  new Error("wrapped", { cause: new Error("MCP prepared receipt unavailable") }),
  { message: "MCP prepared receipt unavailable", authorization: "Bearer test-secret" },
  "MCP prepared receipt unavailable",
  null,
])("managed scanner keeps unknown failures unclassified without error data: %#", async (error) => {
  const warning = spyOn(log, "warn").mockImplementation(() => {});
  const scanner = makeManagedSessionScanner({
    store: { pending: async () => [{ sessionId: "pending" }] },
    runtime: {
      async runOnce() {
        throw error;
      },
    },
  });
  try {
    await scanner.runOnce();
    expect(warning.mock.calls).toEqual([
      [{ reason: "unclassified" }, "MCP managed session reconciliation unavailable"],
    ]);
  } finally {
    warning.mockRestore();
  }
});

test.each([
  [Code.FailedPrecondition, "rpc_failed_precondition"],
  [Code.Unavailable, "rpc_unavailable"],
  [Code.Unauthenticated, "rpc_unauthenticated"],
  [0, "unclassified"],
  [17, "unclassified"],
  [NaN, "unclassified"],
  [1.5, "unclassified"],
])("managed scanner logs only a fixed RPC reason for code %s", async (code, reason) => {
  const error = new ConnectError(
    "Bearer test-secret https://example.test/?token=test-secret",
    Code.Unknown,
    { authorization: "Bearer test-secret" },
  );
  // The wire boundary can supply an invalid numeric code despite the declared enum type.
  Object.defineProperty(error, "code", { value: code });
  const warning = spyOn(log, "warn").mockImplementation(() => {});
  const driven: string[] = [];
  const scanner = makeManagedSessionScanner({
    store: { pending: async () => [{ sessionId: "pending" }, { sessionId: "later" }] },
    runtime: {
      async runOnce(id) {
        driven.push(id);
        if (id === "pending") throw error;
      },
    },
  });
  try {
    await scanner.runOnce();
    expect(driven).toEqual(["pending", "later"]);
    expect(warning.mock.calls).toEqual([
      [{ reason }, "MCP managed session reconciliation unavailable"],
    ]);
  } finally {
    warning.mockRestore();
  }
});
