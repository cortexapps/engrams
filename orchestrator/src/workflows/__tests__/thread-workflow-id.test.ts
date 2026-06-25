/**
 * Thread-reuse epoch selection (ADR 0059 Invariant 4) — pure, unit-tested.
 */

import { expect, test, describe } from "bun:test";
import { selectThreadWorkflowId } from "../thread-workflow-id.ts";

describe("selectThreadWorkflowId", () => {
  test("fresh OR live thread → epoch 0 (no suffix)", async () => {
    expect(await selectThreadWorkflowId("HASH", async () => false)).toBe("task:HASH");
  });

  test("completed thread → successor epoch", async () => {
    const terminal = new Set(["task:HASH"]);
    expect(await selectThreadWorkflowId("HASH", async (id) => terminal.has(id))).toBe("task:HASH#1");
  });

  test("multiple prior epochs terminal → next free", async () => {
    const terminal = new Set(["task:HASH", "task:HASH#1", "task:HASH#2"]);
    expect(await selectThreadWorkflowId("HASH", async (id) => terminal.has(id))).toBe("task:HASH#3");
  });
});
