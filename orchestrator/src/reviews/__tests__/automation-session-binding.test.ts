import { describe, expect, test } from "bun:test";

import { makeAutomationReviewSessionBinding } from "../automation-session-binding.ts";

describe("makeAutomationReviewSessionBinding", () => {
  test("records the worker against the run with keep=false and a no-op remove", async () => {
    const rows: unknown[] = [];
    const binding = makeAutomationReviewSessionBinding("autorun:auto-1:github:d1", "finder_session", {
      async recordSessionBinding(input) {
        rows.push(input);
      },
    });

    await binding.record("s-finder", "finder", "ignored-legacy-id");
    await binding.remove("s-finder");

    expect(rows).toEqual([
      {
        sessionId: "s-finder",
        runId: "autorun:auto-1:github:d1",
        blockId: "finder_session",
        role: "finder",
        keep: false,
      },
    ]);
  });
});
