import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { getSpecMessages, postSpecMessage, useSendSpecMessage } from "./useSpecMessages";

afterEach(() => vi.unstubAllGlobals());

describe("spec message clients", () => {
  test("reads attributed messages, preserves the after cursor, and builds the prompt map", async () => {
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        jsonResponse({
          messages: [
            {
              prompt_id: "prompt-1",
              author: { id: "priya", name: "Priya" },
              text: "Keep one parent bucket.",
              created_at: "2026-08-13T10:00:00.000Z",
            },
          ],
        }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await getSpecMessages("spec/1", "prompt 0");

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec%2F1/messages?after=prompt%200",
      expect.objectContaining({ credentials: "include" }),
    );
    expect(result.messages[0]).toEqual({
      promptId: "prompt-1",
      author: { id: "priya", name: "Priya" },
      text: "Keep one parent bucket.",
      createdAt: "2026-08-13T10:00:00.000Z",
    });
    expect(result.byPromptId.get("prompt-1")).toBe(result.messages[0]);
  });

  test("posts only clean message text and returns the prompt id", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(jsonResponse({ prompt_id: "prompt-2" }, 202)));
    vi.stubGlobal("fetch", fetchMock);

    await expect(postSpecMessage("spec-1", "One shared limit.")).resolves.toEqual({
      promptId: "prompt-2",
    });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/specs/spec-1/messages",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ message: "One shared limit." }),
      }),
    );
  });

  test("invalidates every message cursor after a successful send", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(jsonResponse({ prompt_id: "prompt-3" }, 202))),
    );
    const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");
    const wrapper = ({ children }: { children: ReactNode }) =>
      createElement(QueryClientProvider, { client }, children);
    const { result } = renderHook(() => useSendSpecMessage("spec-1"), { wrapper });

    await result.current.mutateAsync("Write this down.");
    await waitFor(() =>
      expect(invalidate).toHaveBeenCalledWith({ queryKey: ["spec", "spec-1", "messages"] }),
    );
    client.clear();
  });
});

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}
