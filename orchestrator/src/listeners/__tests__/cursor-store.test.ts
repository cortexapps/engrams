import { describe, expect, test } from "bun:test";

import { makeInMemoryCursorStore } from "../cursor-store.ts";

describe("CursorStore contract", () => {
  test("missing cursors start at -1 and remain isolated by session and consumer", async () => {
    const store = makeInMemoryCursorStore();

    expect(await store.get("session-1", "tool-dispatch")).toBe(-1n);
    await store.set("session-1", "tool-dispatch", 7n);

    expect(await store.get("session-1", "tool-dispatch")).toBe(7n);
    expect(await store.get("session-1", "slack")).toBe(-1n);
    expect(await store.get("session-2", "tool-dispatch")).toBe(-1n);
  });
});
