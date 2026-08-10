import { afterEach, describe, expect, it, vi } from "vitest";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import { createElement, type PropsWithChildren } from "react";

import { restoreSpecSection, type SpecRequestError, useSpecRead } from "./useSpecRead";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("restoreSpecSection", () => {
  it("returns the new pre-restore checkpoint from a forward restore", async () => {
    const responseBody = {
      applied: true,
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
    if (!result.applied) throw new Error("The restore response must be applied");
    expect(result.checkpoint.reason).toBe("before_restore");
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec-1/restore",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ checkpointId: "checkpoint-1", sectionId: "context" }),
      }),
    );
  });

  it("keeps the response status for a restore that races with publish", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(null, { status: 409 }))),
    );

    await expect(restoreSpecSection("spec-1", "checkpoint-1", "context")).rejects.toMatchObject({
      status: 409,
    } satisfies Partial<SpecRequestError>);
  });
});

describe("useSpecRead", () => {
  it("polls a foreground draft until the server returns the published checkpoint", async () => {
    vi.useFakeTimers();
    const draft = specReadResponse("draft");
    const published = specReadResponse("published");
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(draft))
      .mockResolvedValue(jsonResponse(published));
    vi.stubGlobal("fetch", fetchMock);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client: queryClient }, children);

    const { result } = renderHook(() => useSpecRead("spec-1"), { wrapper });
    await vi.waitFor(() => expect(result.current.data?.spec.lifecycle).toBe("draft"));
    await vi.advanceTimersByTimeAsync(5_000);
    await vi.waitFor(() => expect(result.current.data?.spec.lifecycle).toBe("published"));
    expect(fetchMock).toHaveBeenCalledTimes(2);

    queryClient.clear();
  });
});

function specReadResponse(lifecycle: "draft" | "published") {
  const published = lifecycle === "published";
  return {
    spec: {
      id: "spec-1",
      title: "Polling design",
      lifecycle,
      sessionId: null,
      publishedCheckpointId: published ? "checkpoint-1" : null,
      publishedAt: published ? "2026-08-10T01:00:00.000Z" : null,
    },
    checkpoints: [],
    publishedCheckpoint: published
      ? {
          id: "checkpoint-1",
          label: "Published",
          authorUserId: "member-1",
          reason: "publish",
          docSeq: "1",
          createdAt: "2026-08-10T01:00:00.000Z",
          markdown: "## Context\n\nPublished.\n",
          sections: [{ id: "context", title: "Context" }],
        }
      : null,
  };
}

function jsonResponse(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}
