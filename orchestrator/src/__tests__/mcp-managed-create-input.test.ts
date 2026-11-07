import { expect, test } from "bun:test";
import {
  managedCreateInputDigest,
  managedCreationInput,
  frozenNativeManifest,
} from "../mcp/managed-create-input.ts";
test("managed creation digest matches the Rust exact-wire golden vector", () => {
  expect(
    managedCreateInputDigest({
      protocol: 1,
      nativeVersion: 1,
      taskId: "task-a",
      nativeDigest: `sha256:${"a".repeat(64)}`,
      creationRequest: new Uint8Array([1, 2, 3]),
    }),
  ).toBe("sha256:fa03d5903a5d81b1127d47cc331d0d833eb68f5fe42012397d86958bf5b995b5");
});
test("creation association retains native bytes and excludes credential bytes", () => {
  const input = {
    requestedSessionId: crypto.randomUUID(),
    imageUri: "image",
    mode: "agent",
    harness: "claude",
    harnessMode: "plan",
    harnessEnv: { ENGRAM_TOOLS: "[ ]", NATIVE_TOKEN: "fixture-sensitive-value" },
  };
  const prepared = managedCreationInput(input, {
    taskId: "task",
    actorId: "actor",
    inputId: "source",
  });
  expect(prepared.identity.writeEligibility).toBe("read_only");
  expect(prepared.nativeDigest).toBe(frozenNativeManifest("[ ]").digest);
  expect(JSON.stringify(prepared.identity)).not.toContain("fixture-sensitive-value");
  expect(prepared.request.inputDigest).toBe(managedCreateInputDigest(prepared.request));
  expect(
    managedCreationInput(
      { ...input, harnessEnv: { ...input.harnessEnv, ENGRAM_TOOLS: "[]" } },
      { taskId: "task", actorId: "actor", inputId: "source" },
    ).request.inputDigest,
  ).not.toBe(prepared.request.inputDigest);
  expect(() =>
    frozenNativeManifest(
      JSON.stringify([
        { name: "mcp_injected", description: "", inputSchema: {}, execution: "sync" },
      ]),
    ),
  ).toThrow();
});
