import { afterEach, describe, expect, it, vi } from "vitest";

import { restoreSpecSection } from "./useSpecRead";

afterEach(() => vi.unstubAllGlobals());

describe("restoreSpecSection", () => {
  it("returns the new pre-restore checkpoint from a forward restore", async () => {
    const responseBody = {
      checkpoint: {
        id: "checkpoint-before-restore",
        label: "Before restore of context",
        authorUserId: "member-1",
        reason: "before_restore",
        docSeq: "4",
        createdAt: "2026-08-10T01:00:00.000Z",
        markdown: "## Context\n\nCurrent state.\n",
        sections: [{ id: "context", title: "Context" }],
      },
      newRev: "5",
    };
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        new Response(JSON.stringify(responseBody), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await restoreSpecSection("spec-1", "checkpoint-1", "context");

    expect(result).toEqual(responseBody);
    expect(result.checkpoint.reason).toBe("before_restore");
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec-1/restore",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ checkpointId: "checkpoint-1", sectionId: "context" }),
      }),
    );
  });
});
