import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import { createElement, type PropsWithChildren } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useSpecDecisions } from "./useSpecDecisions";

afterEach(() => vi.unstubAllGlobals());

describe("useSpecDecisions", () => {
  it("reads the published decision record without changing actor or time fields", async () => {
    const decisions = [
      {
        id: "settle-1",
        kind: "section_settled" as const,
        sectionId: "data",
        sectionTitle: "Data model",
        actor: { id: "priya", name: "Priya Raman" },
        decidedAt: "2026-08-13T18:00:00.000Z",
      },
      {
        id: "question-1",
        kind: "question_resolved" as const,
        sectionId: "data",
        sectionTitle: "Data model",
        question: "Which lock coordinates writers?",
        resolutionLink: "section:data@resolution",
        actor: { id: null, name: "actor unknown" },
        decidedAt: "2026-08-13T18:01:00.000Z",
      },
    ];
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        new Response(JSON.stringify({ decisions }), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: PropsWithChildren) =>
      createElement(QueryClientProvider, { client: queryClient }, children);

    const { result } = renderHook(() => useSpecDecisions("spec/id"), { wrapper });

    await vi.waitFor(() => expect(result.current.data).toEqual(decisions));
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/specs/spec%2Fid/decisions", expect.any(Object));
    queryClient.clear();
  });
});
