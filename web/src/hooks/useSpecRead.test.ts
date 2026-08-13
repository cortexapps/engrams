import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import { createElement, type PropsWithChildren } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  restoreSpecSection,
  setSpecSectionState,
  type SpecRequestError,
  useSetSpecSectionState,
  useSpecRail,
  useSpecRead,
} from "./useSpecRead";

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
  it("polls a foreground drafting spec until the server returns the published checkpoint", async () => {
    vi.useFakeTimers();
    const drafting = specReadResponse("drafting");
    const published = specReadResponse("published");
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(drafting))
      .mockResolvedValue(jsonResponse(published));
    vi.stubGlobal("fetch", fetchMock);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client: queryClient }, children);

    const { result } = renderHook(() => useSpecRead("spec-1"), { wrapper });
    await vi.waitFor(() => expect(result.current.data?.spec.phase).toBe("drafting"));
    await vi.advanceTimersByTimeAsync(5_000);
    await vi.waitFor(() => expect(result.current.data?.spec.phase).toBe("published"));
    expect(fetchMock).toHaveBeenCalledTimes(2);

    queryClient.clear();
  });
});

function specReadResponse(phase: "drafting" | "published") {
  const published = phase === "published";
  return {
    spec: {
      id: "spec-1",
      title: "Polling design",
      phase,
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

describe("setSpecSectionState", () => {
  it("sends one stable action ID and returns the transcript chip", async () => {
    const responseBody = {
      chip: {
        kind: "spec_section_state_changed",
        specId: "spec-1",
        sectionId: "design",
        sectionTitle: "Design",
        before: { state: "proposed", naReason: null },
        after: { state: "settled", naReason: null },
        undo: {
          kind: "restore_section_state",
          specId: "spec-1",
          sectionId: "design",
          expected: { state: "settled", naReason: null },
          restore: { state: "proposed", naReason: null },
        },
      },
      rail: { sections: [], completeness: { complete: 1, total: 1 } },
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

    const result = await setSpecSectionState(
      "spec-1",
      "design",
      "settled",
      undefined,
      "00000000-0000-4000-8000-000000001118",
    );

    expect(result).toEqual(responseBody);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec-1/sections/design/state",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({
          actionId: "00000000-0000-4000-8000-000000001118",
          state: "settled",
        }),
      }),
    );
  });

  it("keeps its action ID across a transport retry and invalidates server state", async () => {
    const responseBody = {
      chip: {
        kind: "spec_section_state_changed",
        specId: "spec-1",
        sectionId: "design",
        sectionTitle: "Design",
        before: { state: "proposed", naReason: null },
        after: { state: "settled", naReason: null },
        undo: {
          kind: "restore_section_state",
          specId: "spec-1",
          sectionId: "design",
          expected: { state: "settled", naReason: null },
          restore: { state: "proposed", naReason: null },
        },
      },
      rail: { sections: [], completeness: { complete: 1, total: 1 } },
    };
    const fetchMock = vi
      .fn()
      .mockRejectedValueOnce(new Error("The connection closed."))
      .mockResolvedValue(jsonResponse(responseBody));
    vi.stubGlobal("fetch", fetchMock);
    const queryClient = new QueryClient({
      defaultOptions: { mutations: { retry: 1, retryDelay: 0 }, queries: { retry: false } },
    });
    const invalidate = vi.spyOn(queryClient, "invalidateQueries");
    const wrapper = ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client: queryClient }, children);
    const actionId = "00000000-0000-4000-8000-000000001119";
    const { result } = renderHook(() => useSetSpecSectionState("spec-1"), { wrapper });

    act(() => {
      result.current.mutate({ sectionId: "design", state: "settled", actionId });
    });

    await vi.waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(fetchMock).toHaveBeenCalledTimes(2);
    for (const call of fetchMock.mock.calls) {
      const init = call[1] as RequestInit;
      expect(JSON.parse(String(init.body))).toMatchObject({ actionId });
    }
    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["spec", "spec-1"] });
    queryClient.clear();
  });
});

describe("useSpecRail", () => {
  it("polls server state while the spec is drafting", async () => {
    vi.useFakeTimers();
    const first = { sections: [], completeness: { complete: 0, total: 1 } };
    const second = { sections: [], completeness: { complete: 1, total: 1 } };
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ rail: first }))
      .mockResolvedValue(jsonResponse({ rail: second }));
    vi.stubGlobal("fetch", fetchMock);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    queryClient.setQueryData(["spec", "spec-1"], specReadResponse("drafting"));
    const wrapper = ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client: queryClient }, children);
    const { result } = renderHook(() => useSpecRail("spec-1"), { wrapper });

    await vi.waitFor(() => expect(result.current.data?.completeness.complete).toBe(0));
    await vi.advanceTimersByTimeAsync(5_000);
    await vi.waitFor(() => expect(result.current.data?.completeness.complete).toBe(1));
    expect(fetchMock).toHaveBeenCalledTimes(2);
    queryClient.clear();
  });
});
