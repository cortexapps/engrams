import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { useSpecTicketCommand, useSpecTickets } from "./useSpecTickets";

const SPEC_ID = "spec-1";

const EMPTY_TREE = {
  specId: SPEC_ID,
  checkpointId: "checkpoint-1",
  docSeq: "18",
  publishedAt: null,
  sections: [],
  tickets: [],
  unattachedQuestions: [],
};

function stubFetch() {
  const calls: Array<{ url: string; method: string; body: unknown }> = [];
  const fetchMock = vi.fn(async (url: string | URL, init?: RequestInit) => {
    calls.push({
      url: String(url),
      method: init?.method ?? "GET",
      body: init?.body === undefined ? undefined : JSON.parse(String(init.body)),
    });
    return new Response(JSON.stringify(EMPTY_TREE), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fetchMock);
  return calls;
}

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  return ({ children }: { children: ReactNode }) =>
    createElement(QueryClientProvider, { client }, children);
}

afterEach(() => vi.unstubAllGlobals());

describe("the ticket tree hooks", () => {
  // The request helper already carries `/api/v1`. A second copy here answered
  // the orchestrator's catch-all 404, and only a real browser showed it.
  test("the read path is prefixed exactly once", async () => {
    const calls = stubFetch();
    const { result } = renderHook(() => useSpecTickets(SPEC_ID), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(calls[0]?.url).toBe("/api/v1/specs/spec-1/tickets");
  });

  test("every command hits its own path, prefixed exactly once", async () => {
    const calls = stubFetch();
    const { result } = renderHook(() => useSpecTicketCommand(SPEC_ID), { wrapper: wrapper() });

    await result.current.mutateAsync({
      kind: "add",
      parentId: null,
      title: "New ticket",
      body: "",
      sectionId: "sec-data",
    });
    await result.current.mutateAsync({ kind: "update", id: "t1", title: "Renamed" });
    await result.current.mutateAsync({ kind: "delete", id: "t1" });
    await result.current.mutateAsync({ kind: "move", id: "t1", parentId: "t2", index: 0 });
    await result.current.mutateAsync({
      kind: "split",
      id: "t1",
      parts: [
        { title: "a", body: "a" },
        { title: "b", body: "b" },
      ],
    });
    await result.current.mutateAsync({ kind: "merge", targetId: "t2", sourceIds: ["t1"] });

    expect(calls.map((call) => `${call.method} ${call.url}`)).toEqual([
      "POST /api/v1/specs/spec-1/tickets",
      "PATCH /api/v1/specs/spec-1/tickets/t1",
      "DELETE /api/v1/specs/spec-1/tickets/t1",
      "POST /api/v1/specs/spec-1/tickets/t1/move",
      "POST /api/v1/specs/spec-1/tickets/t1/split",
      "POST /api/v1/specs/spec-1/tickets/t2/merge",
    ]);
    expect(calls[3]?.body).toEqual({ parentId: "t2", index: 0 });
    expect(calls[5]?.body).toEqual({ sourceIds: ["t1"] });
  });
});
